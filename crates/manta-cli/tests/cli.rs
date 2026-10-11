use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    // MAN-261: `run`/`soak`/`doctor` read `MANTA_CONFIG` and
    // `MANTA_<TABLE>_<KEY>` and reject unknown `MANTA_*` names, so a
    // variable in the test runner's own environment must never leak into
    // the child. Tests that exercise the env tier set theirs explicitly.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// MAN-261: a minimal valid `[server]` table -- loopback only, every port 0
/// (multi-agent hygiene: never bind a fixed port).
const SERVER_TOML: &str = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n";

/// The prefix of `run`'s dial guard (MAN-34), pinned since before MAN-261.
const DIAL_GUARD: &str = "--dial-freq-hz is required with --config";

/// Writes `body` to `dir/name` and returns that path.
fn write_cfg(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

/// V1 shortened to 30 s: long enough for its one W1AW CQ spot (emitted ~21 s
/// in, after SPEC §2.1's warmup+confirm floor -- a 15 s or 20 s scene emits
/// none), yet about four times quicker to replay than the full 120 s.
fn short_v1() -> manta_testkit::vectors::VectorSpec {
    manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    }
}

/// Writes `spec`'s fixture set (`<name>.wav` plus its `<name>.json`
/// sidecar) into `dir` and returns the WAV's path.
fn write_fixture(dir: &Path, spec: &manta_testkit::vectors::VectorSpec) -> PathBuf {
    manta_testkit::vectors::write_fixture_set(spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

/// `dir/v1.wav` (+ sidecar, `center_freq_hz` 14 000 000): [`short_v1`].
fn v1_fixture(dir: &Path) -> PathBuf {
    write_fixture(dir, &short_v1())
}

/// The spot objects from `run --json`'s stdout: each spot is printed as a
/// `{"spot": {...}}` line, interleaved with `DecoderEvent` lines.
fn run_spots(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("spot").cloned())
        .collect()
}

/// Asserts a `run --json` exited 0 with exactly one spot, and returns that
/// spot's `freq_hz`.
fn single_spot_freq(out: &Output) -> f64 {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    let spots = run_spots(&out.stdout);
    assert_eq!(spots.len(), 1, "spots: {spots:?}; stderr: {stderr}");
    spots[0]["freq_hz"].as_f64().unwrap()
}

/// Asserts `freq_hz == base * (1 + ppm * 1e-6)` to within 1 mHz.
fn assert_ppm_scaled(freq_hz: f64, base: f64, ppm: f64, what: &str) {
    let expected = base * (1.0 + ppm * 1e-6);
    assert!(
        (freq_hz - expected).abs() < 1e-3,
        "{what}: expected {expected} (= {base} x (1 + {ppm}e-6)), got {freq_hz}"
    );
}

/// Asserts a `decode --json` exited 0, and returns its report's `spots`.
fn decode_spots(out: &Output) -> Vec<serde_json::Value> {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    report["spots"].as_array().unwrap().clone()
}

/// Asserts `out` failed with every `needle` on stderr, and that it failed
/// before touching the (nonexistent) WAV: `run`/`soak`/`doctor` report a
/// missing source as `Failed to open WAV file: ...`, `decode` as
/// `open WAV <path>`, both ending in ENOENT.
fn assert_rejected_before_source_io(what: &str, out: &Output, needles: &[&str]) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{what}: expected a failure");
    for needle in needles {
        assert!(stderr.contains(needle), "{what}: no {needle:?} in {stderr}");
    }
    // MAN-126: `doctor` reports a source that will not open as a check
    // line on stdout, so stdout must be clean too.
    let stdout = String::from_utf8_lossy(&out.stdout);
    for io in ["open WAV", "No such file", "os error 2"] {
        assert!(
            !stderr.contains(io),
            "{what}: the config must be rejected before any source I/O: {stderr}"
        );
        assert!(
            !stdout.contains(io),
            "{what}: the config must be rejected before any source I/O: {stdout}"
        );
    }
}

/// A UDP port nothing answers on: `doctor --ntp-server 127.0.0.1:<it>`
/// keeps the clock check off the internet (MAN-126).
fn closed_ntp_server() -> String {
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("127.0.0.1:{port}")
}

/// `run`, `decode`, `soak` and `doctor`, each given `--config cfg` and a
/// nonexistent WAV, so a config error is the only way to fail before I/O.
fn every_config_command(cfg: &Path) -> [(&'static str, Command); 4] {
    let mut run = manta();
    run.args([
        "run",
        "--source",
        "/nonexistent.wav",
        "--dial-freq-hz",
        "14025000",
        "--config",
    ])
    .arg(cfg);
    let mut decode = manta();
    decode
        .args(["decode", "--config"])
        .arg(cfg)
        .arg("/nonexistent.wav");
    let mut soak = manta();
    soak.args([
        "soak",
        "--duration",
        "2",
        "--source",
        "/nonexistent.wav",
        "--config",
    ])
    .arg(cfg);
    let mut doctor = manta();
    doctor
        .args([
            "doctor",
            "--duration",
            "3",
            "--source",
            "/nonexistent.wav",
            "--config",
        ])
        .arg(cfg);
    [
        ("run", run),
        ("decode", decode),
        ("soak", soak),
        ("doctor", doctor),
    ]
}

/// `dir` joined with a file name that is not valid UTF-8. The file is never
/// created: macOS's APFS refuses non-UTF-8 names outright.
#[cfg(unix)]
fn non_utf8_path(dir: &Path, name: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt as _;
    dir.join(std::ffi::OsStr::from_bytes(name))
}

/// MAN-261 scenario 4: an ordinary error (exit 1), never a panic (exit 101).
#[cfg(unix)]
fn assert_exit_1_without_panic(out: &Output) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert_ne!(out.status.code(), Some(101), "stderr: {stderr}");
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
}

/// SPEC §2.1's ~2.05 s mandatory warmup(750 hops)+confirm(19 hops) floor
/// deterministically loses this 15 s scene's leading "CQ " before the real
/// detector ever promotes a track -- not a bug, same structural cause as
/// `golden_v1.rs`/`pipeline.rs`'s V1-based tests (see those files' doc
/// comments). A 15 s scene loses the ~2.05 s absolute prefix as a much
/// larger fraction than V1's full 120 s gate. Measured empirically (Task 11
/// Step 0): CER = 0.1304, deterministic (V1's fixed `noise_seed`). 0.17
/// gives headroom above that floor. See
/// docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md.
#[test]
fn gen_then_decode_prints_text() {
    let dir = tempfile::tempdir().unwrap();
    // Generate a short fixture through the library (fast), decode via the CLI.
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .arg("decode")
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let cer_val = manta_testkit::cer::cer(&manifest.keyed_texts[0], text.trim());
    assert!(
        cer_val < 0.17,
        "expected CER < 0.17 (measured floor 0.1304), got {cer_val:.4}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        text.trim()
    );
}

#[test]
fn gen_subcommand_writes_fixture_set() {
    let dir = tempfile::tempdir().unwrap();
    // NOTE: full 120 s V1 — this is also the fixture-generation smoke test.
    let out = manta()
        .args(["gen", "v1", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("v1.wav").exists());
    assert!(dir.path().join("v1.json").exists());
    assert!(dir.path().join("v1.manifest.json").exists());
}

#[test]
fn unknown_vector_errors() {
    let dir = tempfile::tempdir().unwrap();
    let out = manta()
        .args(["gen", "v99", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// `Engine::Hsmm` is a fully implemented, reviewed engine since Task 8
/// (`TrackDecoder::push_hop_hsmm`) and, as of Task 11, is no longer
/// rejected by `parse_engine` on any command: `--engine hsmm` must run the
/// real decode pipeline end to end (not just parse), the same as `legacy`/
/// `edge-legacy`.
#[test]
fn decode_engine_hsmm_runs_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["decode", "--json", "--engine", "hsmm"])
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "must not panic; stderr: {stderr}"
    );
    assert!(
        out.status.success(),
        "--engine hsmm must run successfully; stderr: {stderr}"
    );
    // A parseable DecodeReport proves the hsmm engine ran the full
    // decode -> JSON-report pipeline, not just that clap accepted the flag.
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report["events"].is_array());
}

#[test]
fn run_is_the_canonical_daemon_verb() {
    // MAN-77 scenario 1. Repro on e398d46: `manta run --help` exited 2 with
    // "error: unrecognized subcommand 'run'".
    let out = manta().args(["run", "--help"]).output().unwrap();
    assert!(out.status.success(), "manta run --help should succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage: manta run"), "stdout: {stdout}");

    let top = manta().arg("--help").output().unwrap();
    let top = String::from_utf8_lossy(&top.stdout);
    // `run` is listed as a command; `listen` appears only as its alias.
    assert!(top.contains("  run "), "top-level help: {top}");
    assert!(top.contains("[alias: listen]"), "top-level help: {top}");
}

#[test]
fn listen_is_still_accepted_as_an_alias_of_run() {
    // The ticket's "existing scripts don't break silently" requirement.
    let out = manta().args(["listen", "--help"]).output().unwrap();
    assert!(
        out.status.success(),
        "manta listen --help should still succeed"
    );
}

#[test]
fn decode_and_gen_are_unaffected_by_the_verb_promotion() {
    // MAN-77 scenario 2, asserted explicitly rather than left implicit.
    for sub in ["decode", "gen"] {
        let out = manta().args([sub, "--help"]).output().unwrap();
        assert!(out.status.success(), "manta {sub} --help should succeed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("Usage: manta {sub}")),
            "{sub}: {stdout}"
        );
    }
}

#[test]
fn config_is_the_canonical_daemon_config_flag() {
    // Repro on e398d46: "error: unexpected argument '--config' found".
    // MAN-261: the config is loaded (strictly) first, so it must be a real
    // file with a valid [server]; the dial guard then fires after that
    // successful load and before any source I/O (the WAV stays
    // nonexistent). The --dial-freq-hz error proves --config was accepted
    // and routed to the same field --server-config used to reach.
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), "manta.toml", SERVER_TOML);
    let out = manta()
        .args(["run", "--source", "/nonexistent.wav", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
    assert!(stderr.contains(DIAL_GUARD), "stderr: {stderr}");
    // The error text must name the new flag, not the old one.
    assert!(stderr.contains("--config"), "stderr: {stderr}");
    assert!(
        !stderr.contains("--server-config"),
        "stale flag name: {stderr}"
    );
}

#[test]
fn server_config_is_still_accepted_as_a_hidden_alias_of_config() {
    // A real config (MAN-261: loaded before the dial guard fires).
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), "manta.toml", SERVER_TOML);
    let out = manta()
        .args(["run", "--source", "/nonexistent.wav", "--server-config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
    assert!(stderr.contains(DIAL_GUARD), "stderr: {stderr}");

    // Hidden: help advertises the canonical name only.
    let help = manta().args(["run", "--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--config <CONFIG>"), "help: {help}");
    assert!(
        !help.contains("--server-config"),
        "deprecated flag advertised: {help}"
    );
}

#[test]
fn deprecated_daemon_spelling_warns_on_stderr_and_names_the_replacement() {
    let out = manta()
        .args([
            "listen",
            "--source",
            "/nonexistent.wav",
            "--server-config",
            "/nonexistent.toml",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("`manta run --config`"), "stderr: {stderr}");
    assert!(stderr.contains("`--config`"), "stderr: {stderr}");
}

#[test]
fn capture_rate_hz_that_does_not_evenly_divide_the_source_rate_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 48_000.0; // AudioIqSource requires exactly 48000 Hz native
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--capture-rate-hz", "20000"]) // 48000/20000 is not an integer
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--capture-rate-hz") || stderr.contains("power of two"),
        "stderr: {stderr}"
    );
}

#[test]
fn capture_rate_hz_that_divides_evenly_decimates_and_still_decodes() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 48_000.0; // AudioIqSource requires exactly 48000 Hz native
    spec.duration_s = 10.0; // short scene, this test only proves the wiring runs end-to-end
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--capture-rate-hz", "24000"]) // 48000 -> 24000, factor 2
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn capture_rate_hz_replays_a_2channel_iq_wav_through_wav_iq_source() {
    // MAN-169 round-2 Codex finding: `open_audio_source` used to route
    // every `--source <path>.wav` through `AudioIqSource::from_wav_file`
    // unconditionally, which hard-rejects every rate but 48000 Hz -- so a
    // 96/192 kS/s raw complex-IQ replay (the format `decode`/`oracle`
    // already read directly) could never reach `--capture-rate-hz`'s
    // decimation wrapper via the CLI at all; only golden-vector tests that
    // called `Decimator` directly (`golden_decimated_capture.rs`) ever
    // exercised that combination. This drives the real `run --source ...
    // --capture-rate-hz ...` CLI path end-to-end against a genuine
    // 2-channel 96 kHz IQ WAV to prove `open_audio_source` now detects the
    // 2-channel case and routes it through `WavIqSource` instead, unlocking
    // decimated file replay the same way it already works for live SDR
    // sources.
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 96_000.0;
    spec.duration_s = 10.0; // short scene, this test only proves the wiring runs end-to-end
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--source-iq", "--capture-rate-hz", "48000"]) // 96000 -> 48000, factor 2
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn without_source_iq_a_2channel_wav_is_still_treated_as_stereo_audio() {
    // MAN-169 round-4 Codex finding (Finding A): channel count alone can't
    // distinguish a genuine 2-channel raw-IQ capture from an ordinary
    // stereo real-audio recording -- both `WavIqSource` and `AudioIqSource`
    // accept 2-channel WAVs. Without `--source-iq`, `--source` must always
    // go through `AudioIqSource::from_wav_file` (the pre-round-2, and
    // pre-this-PR, default), never `WavIqSource`. Proven indirectly: v1()'s
    // default fs is 96000 Hz, and `AudioIqSource::from_wav_file` hard-
    // rejects every rate but 48000 -- so this must fail with that source's
    // own "48000" error, not a `WavIqSource`-shaped success or a different
    // error, proving the 2-channel WAV was never silently reinterpreted as
    // IQ.
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1(); // fs=96_000, 2-channel WAV, no --source-iq
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("48000"),
        "expected AudioIqSource's rate-mismatch error, got: {stderr}"
    );
    assert!(
        !stderr.contains("IQ WAV must have"),
        "must not go through WavIqSource without --source-iq: {stderr}"
    );
}

#[test]
fn config_does_not_require_dial_freq_hz_for_an_iq_wav_with_a_real_sidecar() {
    // MAN-169 round-3 Codex finding: `has_rf_aware_source` (the gate behind
    // `--dial-freq-hz is required with --config`) only checked kiwi/soapy/
    // hpsdr CLI flags -- a 2-channel IQ WAV replay with a real
    // `<stem>.json` sidecar center frequency (the same file format Task 2's
    // `WavIqSource` round-2 fix unlocked for --capture-rate-hz) was still
    // wrongly rejected as "not RF-aware" and forced a redundant
    // --dial-freq-hz, even though the source already reports a real RF
    // center via WavIqSource::center_freq_hz(). This proves the gate no
    // longer fires for that case. MAN-261: with a real, valid [server]
    // config (loopback, ports 0) the whole run now succeeds -- the file
    // replay ends at EOF -- so the absence of the --dial-freq-hz error is
    // no longer a vacuous pass behind a missing-config failure.
    // Requires --source-iq (round-4: no more channel-count sniffing).
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path()); // fs=96_000, center_freq_hz=14_000_000 (nonzero)
    let cfg = write_cfg(dir.path(), "manta.toml", SERVER_TOML);

    let out = manta()
        .args(["run", "--source"])
        .arg(&wav)
        .args(["--source-iq", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        !stderr.contains("--dial-freq-hz"),
        "RF-awareness gate should not fire for an IQ WAV with a real sidecar: {stderr}"
    );
}

#[test]
fn config_still_requires_dial_freq_hz_for_a_negative_sidecar_center_freq() {
    // MAN-169 round-5 Codex finding: source_iq_has_real_rf_center's old
    // `!= 0.0` check treated a negative center_freq_hz as RF-aware too --
    // an RF dial frequency in this domain is never negative, so a negative
    // sidecar value must still trip the --dial-freq-hz guard, the same as
    // the round-4 zero-sentinel case.
    // MAN-261: a real [server] config, so the guard fires after a
    // successful load rather than behind a missing-file error.
    let dir = tempfile::tempdir().unwrap();
    let mut spec = short_v1();
    spec.center_freq_hz = -1_000_000.0;
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let cfg = write_cfg(dir.path(), "manta.toml", SERVER_TOML);

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--source-iq", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--dial-freq-hz"),
        "a negative sidecar center_freq_hz must still require --dial-freq-hz: {stderr}"
    );
    assert!(stderr.contains(DIAL_GUARD), "stderr: {stderr}");
}

#[test]
fn config_requires_dial_freq_hz_for_an_iq_wav_with_a_zero_sidecar_center() {
    // MAN-169 round-4 Codex finding (Finding B): a `<stem>.json` sidecar
    // existing is not proof its `center_freq_hz` is meaningful --
    // `center_freq_hz: 0.0` is `WavIqSource`'s own "unknown center"
    // sentinel (the same value it reports when there's no sidecar at all),
    // so existence-only checking wrongly bypassed the --dial-freq-hz guard
    // for a source that doesn't actually report a real RF center. This
    // proves the opposite of the sibling "real sidecar" test above: the
    // guard must still fire when the sidecar's value is the zero sentinel.
    // MAN-261: a real [server] config, so the guard fires after a
    // successful load rather than behind a missing-file error.
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        center_freq_hz: 0.0,
        ..short_v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let cfg = write_cfg(dir.path(), "manta.toml", SERVER_TOML);

    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("v1.wav"))
        .args(["--source-iq", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--dial-freq-hz"),
        "a sidecar with center_freq_hz: 0.0 must not bypass the --dial-freq-hz guard: {stderr}"
    );
    assert!(stderr.contains(DIAL_GUARD), "stderr: {stderr}");
}

#[test]
fn capture_rate_hz_rejects_non_finite_and_degenerately_small_values() {
    // MAN-169 whole-branch review finding: a small --capture-rate-hz (e.g.
    // 187.5 Hz, reachable as 48000/256) resolves to a Channelizer with
    // hop=0, which hangs Channelizer::process's read-advancing loop
    // forever. Caught here, at CLI-parse time -- before any source is
    // opened -- via parse_capture_rate_hz's MIN_CAPTURE_RATE_HZ floor, not
    // just later at Decimator::new's own construction-time check.
    // "-inf"/negative values aren't exercised here, same reasoning as
    // hpsdr_rate_rejects_non_finite_values above: clap treats a leading
    // "-" as a new flag rather than this value unless
    // `allow_negative_numbers` is set, which this flag doesn't need since
    // every legitimate rate is positive.
    for bad_rate in ["NaN", "inf", "0", "187.5", "500"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([
                "run",
                "--source",
                "/nonexistent-for-this-test.wav",
                "--capture-rate-hz",
                bad_rate,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--capture-rate-hz {bad_rate} should be rejected before any I/O"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("capture-rate-hz"),
            "expected an explanatory error for --capture-rate-hz {bad_rate}, got: {stderr}"
        );
        assert!(
            !stderr.contains("nonexistent-for-this-test"),
            "should fail at CLI-parse time, before the source file is ever opened: {stderr}"
        );
    }
}

#[test]
fn the_ad_hoc_listen_path_is_not_nagged() {
    // The ticket title keeps `listen` for audio/dev testing, and
    // docs/RUNBOOKS/m1-w1aw-live-copy.md still instructs `listen --device`.
    let out = manta()
        .args(["listen", "--kiwi-host", "example.com"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("deprecated"), "unexpected nag: {stderr}");
}

#[test]
fn deprecation_notices_never_touch_stdout() {
    // AGENTS.md: file input -> byte-identical spot logs. stdout carries the
    // JSON Lines stream; a warning there would corrupt it. Uses the same
    // argv as `deprecated_daemon_spelling_warns_on_stderr_and_names_the_replacement`
    // (which does emit both notices) -- `listen --help` emits no notice at
    // all, so it can't catch an eprintln!->println! regression.
    let out = manta()
        .args([
            "listen",
            "--source",
            "/nonexistent.wav",
            "--server-config",
            "/nonexistent.toml",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("deprecated"), "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated"),
        "test is vacuous unless a notice actually fires; stderr: {stderr}"
    );
}

#[test]
#[cfg(unix)]
fn non_utf8_argv_does_not_panic() {
    // Regression: warn_deprecations() used to scan std::env::args(), which
    // panics on non-UTF-8 argv. It runs as main()'s first statement, before
    // Cli::parse() (which uses args_os() via clap and tolerates non-UTF-8
    // paths) ever sees the argv -- so this must not panic for ANY
    // subcommand, not only the deprecated spellings. Filenames are byte
    // strings on Linux/macOS and need not be UTF-8.
    use std::os::unix::ffi::OsStrExt as _;
    let bad_path = std::ffi::OsStr::from_bytes(b"/tmp/man77-non-utf8-\xff.wav");
    let out = manta().arg("decode").arg(bad_path).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    // The nonexistent (and non-UTF-8-named) file should fail like any other
    // missing file, not crash the argv scan before Cli::parse() runs.
    assert!(!out.status.success());
}

#[test]
fn kiwi_host_without_freq_is_a_clean_error() {
    let out = manta()
        .args(["listen", "--kiwi-host", "example.com"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --kiwi-freq"
    );
}

#[test]
fn server_config_without_dial_freq_for_audio_source_is_a_clean_error() {
    // MAN-261: validated after the config loads and before any source I/O,
    // so the config must be a real file with a valid [server] while the
    // WAV path can stay nonexistent.
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), "manta.toml", SERVER_TOML);
    let out = manta()
        .args(["listen", "--source", "/nonexistent.wav", "--server-config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --dial-freq-hz"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
    assert!(stderr.contains(DIAL_GUARD), "stderr: {stderr}");
}

/// MAN-34 review finding: `manta doctor` hardcoded no dial frequency, so a
/// rig-audio device or WAV diagnosed through it always reported baseband
/// offsets even though `listen`/`soak` accept `--dial-freq-hz` for the same
/// input. The flag must exist on `doctor` and reach the audio source.
#[test]
fn doctor_accepts_and_applies_dial_freq_hz_for_an_audio_source() {
    let help = manta().args(["doctor", "--help"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("--dial-freq-hz"),
        "doctor --help must list --dial-freq-hz"
    );

    // A bad value is rejected at parse time, proving the flag is wired to
    // the same validator `listen` uses.
    let bad = manta()
        .args([
            "doctor",
            "--source",
            "/nonexistent.wav",
            "--dial-freq-hz",
            "not-a-number",
        ])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("dial-freq-hz"),
        "stderr: {}",
        String::from_utf8_lossy(&bad.stderr)
    );

    // A real 48 kHz mono audio WAV: the report's center frequency is the
    // operator's dial frequency, not the audio source's own 0.0.
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("tone.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    for n in 0..(48_000 * 4) {
        let t = n as f32 / 48_000.0;
        w.write_sample((8_000.0 * (2.0 * std::f32::consts::PI * 700.0 * t).sin()) as i16)
            .unwrap();
    }
    w.finalize().unwrap();

    let out = manta()
        .args(["doctor", "--duration", "3", "--json", "--source"])
        .arg(&wav)
        .args(["--dial-freq-hz", "14030000", "--ntp-server"])
        .arg(closed_ntp_server())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("doctor --json not JSON ({e}): stdout={stdout} stderr={stderr}")
    });
    assert_eq!(
        report["center_freq_hz"].as_f64(),
        Some(14_030_000.0),
        "report: {report}"
    );
}

/// SPEC v2 §0/§7: `manta listen` gets the same `--engine` flag `manta
/// decode` already has (Task 6), threaded through to the same
/// `PipelineConfig`/`DecodeConfig` `manta_engine::listen` reads (Task 9).
/// `hsmm` (Task 8, no longer CLI-gated as of Task 11) is included alongside
/// `legacy`/`edge-legacy` -- all three are recognized `Engine` values with
/// no rejection anywhere in this command.
/// A full decode-success run (as `decode_accepts_engine_flag`, Task 9
/// brief, does for `decode`) isn't used here: `--source` requires a real
/// 48 kHz mono audio WAV (`AudioIqSource`, not `decode`'s 96 kHz complex-IQ
/// vector format), and a synthetic clean one hits a pre-existing,
/// `#[ignore]`'d `AudioIqSource`/Hilbert near-DC leakage bug
/// (`manta-engine`'s `listen_decodes_a_clean_real_audio_signal`,
/// <https://github.com/HagaleTechnologies/manta/issues/21>) that spuriously
/// promotes extra tracks -- not something Task 9 should newly depend on
/// being fixed. Instead: for each valid engine value, confirm clap accepts
/// the flag (exit code is NOT clap's arg-error 2) and the run fails for the
/// EXPECTED downstream reason (the nonexistent source file), proving
/// `--engine` parsed successfully and `merge_cli_engine`/
/// `load_decode_config_file` ran without erroring before ever reaching
/// `open_source`.
#[test]
fn listen_accepts_engine_flag_for_every_valid_value() {
    for engine in ["legacy", "edge-legacy", "hsmm"] {
        let out = manta()
            .args(["listen", "--engine", engine, "--source", "/nonexistent.wav"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "{engine}: expected a failure");
        assert_ne!(
            out.status.code(),
            Some(2),
            "{engine}: --engine must not be rejected as a bad argument"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("nonexistent.wav")
                || stderr.contains("No such file")
                // Windows' io::Error Display for ENOENT is "The system
                // cannot find the file specified." rather than "No such
                // file" -- but the underlying OS error code (2) is shared
                // with Unix's ENOENT, so match on that instead of
                // platform-specific wording.
                || stderr.contains("os error 2"),
            "{engine}: expected the nonexistent-source-file error, got: {stderr}"
        );
    }
}

/// Regression, black-box: SPEC v2 §7 requires an explicit `--engine` to
/// override `[decode]`'s `engine` key. An earlier version validated
/// `engine = "hsmm"` at TOML-deserialize time -- BEFORE the CLI override
/// was ever consulted -- so a config file staging `engine = "hsmm"` failed
/// immediately even with `--engine legacy` on the command line, and the
/// override never got a chance to run. As of Task 11 `hsmm` is no longer
/// CLI-gated at all, but the precedence rule this test protects still
/// matters: exercises the actual `manta` subprocess (not just the internal
/// merge functions) both ways -- an explicit `--engine legacy` must beat a
/// hsmm-staged file, and with no override the file's own `hsmm` value must
/// be honored (both cases failing only for the expected, unrelated
/// downstream reason: the nonexistent source file).
#[test]
fn cli_engine_override_beats_a_hsmm_staged_server_config_file() {
    use std::io::Write as _;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(
        f,
        r#"
        [server]
        station_callsign = "W3XYZ"
        [decode]
        engine = "hsmm"
        "#
    )
    .unwrap();
    f.flush().unwrap();

    // With --engine legacy: the override must win over the file's hsmm
    // value and fail only for the expected downstream reason (source file
    // doesn't exist).
    let out = manta()
        .args(["listen", "--engine", "legacy", "--server-config"])
        .arg(f.path())
        .args(["--source", "/nonexistent.wav", "--dial-freq-hz", "14027000"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a failure (nonexistent source)"
    );
    assert_ne!(
        out.status.code(),
        Some(2),
        "--engine must not be rejected as a bad argument"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonexistent.wav")
            || stderr.contains("No such file")
            // Windows' io::Error Display for ENOENT is "The system cannot
            // find the file specified." rather than "No such file" -- but
            // the underlying OS error code (2) is shared with Unix's
            // ENOENT, so match on that instead of platform-specific
            // wording.
            || stderr.contains("os error 2"),
        "expected the nonexistent-source-file error, got: {stderr}"
    );

    // With NO --engine override: the file's own hsmm value is honored (not
    // rejected) and the run still fails only for the same unrelated,
    // expected reason.
    let out = manta()
        .args(["listen", "--server-config"])
        .arg(f.path())
        .args(["--source", "/nonexistent.wav", "--dial-freq-hz", "14027000"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a failure (nonexistent source)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonexistent.wav")
            || stderr.contains("No such file")
            // Windows' io::Error Display for ENOENT is "The system cannot
            // find the file specified." rather than "No such file" -- but
            // the underlying OS error code (2) is shared with Unix's
            // ENOENT, so match on that instead of platform-specific
            // wording.
            || stderr.contains("os error 2"),
        "expected the nonexistent-source-file error, got: {stderr}"
    );
}

#[test]
fn dial_freq_hz_rejects_non_finite_and_non_positive_values() {
    for bad in ["nan", "inf", "-inf", "0", "-14027000"] {
        let out = manta()
            .args([
                "listen",
                "--source",
                "/nonexistent.wav",
                "--dial-freq-hz",
                bad,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--dial-freq-hz {bad} should have been rejected"
        );
    }
}

/// MAN-34, the ticket's Gherkin end-to-end through the compiled binary:
/// `listen --source <48 kHz rig-audio WAV> --dial-freq-hz F --json` reports
/// F + baseband offset, not the bare offset.
///
/// Asserts on `TrackMeta.freq_hz` rather than on an emitted spot: spot
/// emission additionally requires callsign/grammar/repetition validation
/// that a synthetic tone fixture cannot guarantee deterministically, and
/// `Spot.freq_hz` is populated verbatim from `TrackMeta.freq_hz` (times the
/// ppm factor, 1.0 by default) at manta-spot/src/validator.rs:592-599,
/// 723-725. The TrackMeta -> Spot leg is already covered elsewhere
/// (manta-engine's `listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero`).
///
/// `DecoderEvent` is internally tagged (`#[serde(tag = "event")]`,
/// crates/manta-decode/src/events.rs), so each JSON line looks like
/// `{"event":"TrackMeta","track_id":..,"freq_hz":..}` -- not serde's
/// default externally-tagged `{"TrackMeta": {...}}` shape.
#[test]
fn listen_with_dial_freq_hz_reports_absolute_frequency_for_an_audio_source() {
    const CENTER_HZ: f64 = 14_030_000.0;
    const TONE_HZ: f64 = 750.0;
    let fs = 48_000u32;
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("rig-audio.wav");

    let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
    let (env, _keyed) =
        manta_testkit::keyer::key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs as f64, 15.0)
            .unwrap();
    let mut w = hound::WavWriter::create(
        &wav,
        hound::WavSpec {
            channels: 1,
            sample_rate: fs,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    let dphi = std::f64::consts::TAU * TONE_HZ / fs as f64;
    let mut phi = 0.0f64;
    for e in env.iter() {
        w.write_sample(e * phi.cos() as f32).unwrap();
        phi += dphi;
    }
    w.finalize().unwrap();

    let out = manta()
        .args(["listen", "--json", "--dial-freq-hz", "14030000", "--source"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let freqs: Vec<f64> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("TrackMeta"))
        .filter_map(|v| v.get("freq_hz").and_then(|f| f.as_f64()))
        .collect();
    assert!(
        !freqs.is_empty(),
        "expected TrackMeta events, got: {stdout}"
    );
    for f in &freqs {
        assert!(
            (f - CENTER_HZ).abs() < fs as f64 / 2.0,
            "TrackMeta.freq_hz {f} is not an absolute frequency around {CENTER_HZ}"
        );
    }
}

#[test]
fn listen_warns_when_an_audio_source_has_no_dial_freq_hz() {
    // The warning is emitted before any source is opened, so a nonexistent
    // path still provokes it (same technique as the --server-config test).
    let out = manta()
        .args(["listen", "--source", "/nonexistent.wav"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
    assert!(stderr.contains("baseband"), "stderr: {stderr}");
}

#[test]
fn listen_does_not_warn_when_dial_freq_hz_is_supplied() {
    let out = manta()
        .args([
            "listen",
            "--source",
            "/nonexistent.wav",
            "--dial-freq-hz",
            "14030000",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("baseband"),
        "no baseband warning expected with --dial-freq-hz: {stderr}"
    );
}

#[test]
fn soak_dial_freq_hz_rejects_non_finite_and_non_positive_values() {
    for bad in ["nan", "inf", "-inf", "0", "-14027000"] {
        let out = manta()
            .args([
                "soak",
                "--duration",
                "1",
                "--source",
                "/nonexistent.wav",
                "--dial-freq-hz",
                bad,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "soak --dial-freq-hz {bad} should have been rejected"
        );
    }
}

#[test]
fn soak_warns_when_an_audio_source_has_no_dial_freq_hz() {
    let out = manta()
        .args(["soak", "--duration", "1", "--source", "/nonexistent.wav"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--dial-freq-hz"), "stderr: {stderr}");
}

#[test]
fn json_output_is_valid_and_deterministic_across_three_runs() {
    // SPEC §6 CI rule: same binary + same file, 3 runs -> identical output.
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectorspec_short();
    let _ = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let runs: Vec<Vec<u8>> = (0..3)
        .map(|_| {
            let out = manta()
                .args(["decode", "--json"])
                .arg(dir.path().join("v1.wav"))
                .output()
                .unwrap();
            assert!(out.status.success());
            out.stdout
        })
        .collect();
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[1], runs[2]);
    let v: serde_json::Value = serde_json::from_slice(&runs[0]).unwrap();
    assert!(v["text"].is_string());
    assert!(v["freq_hz"].is_f64());
    assert!(v["events"].is_array());
}

/// MAN-29 review round 3: `manta decode` (the primary offline-IQ path) had
/// no `--freq-correction-ppm`, unlike `listen`/`soak` -- a user decoding a
/// recording from a source with a known oscillator correction couldn't use
/// the feature through the CLI at all.
#[test]
fn decode_freq_correction_ppm_shifts_the_reported_freq_hz() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));

    let uncalibrated_out = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(uncalibrated_out.status.success());
    let uncalibrated: serde_json::Value = serde_json::from_slice(&uncalibrated_out.stdout).unwrap();
    let uncalibrated_freq = uncalibrated["freq_hz"].as_f64().unwrap();

    let calibrated_out = manta()
        .args(["decode", "--json", "--freq-correction-ppm", "10"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        calibrated_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&calibrated_out.stderr)
    );
    let calibrated: serde_json::Value = serde_json::from_slice(&calibrated_out.stdout).unwrap();
    let calibrated_freq = calibrated["freq_hz"].as_f64().unwrap();

    let expected = uncalibrated_freq * (1.0 + 10.0 * 1e-6);
    assert!(
        (calibrated_freq - expected).abs() < 1e-3,
        "--freq-correction-ppm 10 should scale freq_hz {uncalibrated_freq} to {expected}, got {calibrated_freq}"
    );
}

/// MAN-29 review round 5: a downward correction (negative ppm) is a normal
/// case the public validation contract explicitly supports
/// (`[-1000, 1000]`), but clap treats a leading-hyphen value as another
/// argument unless `allow_negative_numbers` is set -- so `decode`,
/// `listen`, and `soak` all rejected `--freq-correction-ppm -10` before it
/// ever reached the validator. `decode` is the only one testable without a
/// live device/file, so it stands in for all three.
#[test]
fn decode_accepts_a_negative_freq_correction_ppm() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));

    let out = manta()
        .args(["decode", "--json", "--freq-correction-ppm", "-10"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--freq-correction-ppm -10 should be accepted, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn decode_json_includes_spots_field() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        report.get("spots").is_some_and(|s| s.is_array()),
        "expected a 'spots' array field in decode --json output, got: {report}"
    );
}

/// MAN-28 Watch List: an operator running `manta decode` on a real
/// recording must be able to force-spot a callsign that fails automatic
/// validation, via `--allowlist`. `decode` is the only subcommand
/// testable without a live device/file, same rationale as the
/// freq-correction-ppm CLI tests above.
#[test]
fn decode_allowlist_spots_a_call_that_fails_cty_validation() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 30.0;
    spec.signals[0].text = "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));

    let without_allowlist = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(without_allowlist.status.success());
    let report: serde_json::Value = serde_json::from_slice(&without_allowlist.stdout).unwrap();
    assert!(
        !report["spots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["callsign"] == "QQ9ZZZ"),
        "QQ9ZZZ (unallocated cty prefix) must not spot without --allowlist, got: {report}"
    );

    let with_allowlist = manta()
        .args(["decode", "--json", "--allowlist", "QQ9ZZZ"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        with_allowlist.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&with_allowlist.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&with_allowlist.stdout).unwrap();
    assert!(
        report["spots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["callsign"] == "QQ9ZZZ"),
        "--allowlist QQ9ZZZ should force a spot for QQ9ZZZ, got: {report}"
    );
}

/// MAN-31: an operator must be able to supply the suppression lists from
/// the CLI, not just via the library API -- this is the end-to-end proof
/// the wiring reaches production, not just `PipelineConfig` in isolation.
#[test]
fn decode_blocklist_flag_suppresses_a_callsign() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let blocklist_path = dir.path().join("bad-calls.txt");
    std::fs::write(&blocklist_path, "W1AW\n").unwrap();

    let out = manta()
        .args(["decode", "--json", "--blocklist"])
        .arg(&blocklist_path)
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["spots"].as_array().unwrap().len(),
        0,
        "blocklisted callsign must never be spotted, got: {report}"
    );
}

/// A Windows-authored suppression file commonly starts with a UTF-8 BOM
/// (`\u{feff}`); it must not defeat the first entry's match.
#[test]
fn decode_blocklist_flag_tolerates_a_leading_bom() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let blocklist_path = dir.path().join("bad-calls.txt");
    std::fs::write(&blocklist_path, "\u{feff}W1AW\n").unwrap();

    let out = manta()
        .args(["decode", "--json", "--blocklist"])
        .arg(&blocklist_path)
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["spots"].as_array().unwrap().len(),
        0,
        "a BOM-prefixed blocklist's first entry must still match, got: {report}"
    );
}

#[test]
#[cfg(feature = "soapy")]
fn soapy_driver_without_freq_and_rate_is_a_clean_error() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["listen", "--soapy-driver", "driver=rtlsdr"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --soapy-freq/--soapy-rate"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("soapy-freq")
            || stderr.contains("soapy-rate")
            || stderr.contains("required"),
        "expected an explanatory error, got: {stderr}"
    );
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_host_without_freq_and_rate_is_a_clean_error() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["listen", "--hpsdr-host", "192.168.1.100"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --hpsdr-freq/--hpsdr-rate"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("hpsdr-freq")
            || stderr.contains("hpsdr-rate")
            || stderr.contains("required"),
        "expected an explanatory error, got: {stderr}"
    );
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_flags_are_recognized_by_listen_and_soak() {
    // Flag-recognition smoke test (MAN-51 acceptance): confirms
    // --hpsdr-host/--hpsdr-port/--hpsdr-freq/--hpsdr-rate exist on both
    // subcommands per the ticket's Gherkin -- clap must not reject them as
    // unknown arguments. Checked via --help rather than a real invocation
    // (round-1 review finding): actually running `listen`/`soak` with a
    // plausible LAN host like 192.168.1.100 risks an unbounded hang on any
    // machine where something really answers on that address -- `--help`
    // proves flag recognition with zero I/O. Connecting to a real device
    // is MAN-52's job.
    for sub in ["listen", "soak"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([sub, "--help"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{sub} --help should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        for flag in [
            "--hpsdr-host",
            "--hpsdr-port",
            "--hpsdr-freq",
            "--hpsdr-rate",
        ] {
            assert!(
                stdout.contains(flag),
                "{sub} --help should list {flag}, got: {stdout}"
            );
        }
    }
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_rate_rejects_non_finite_values() {
    // Round-1 review finding: `HpsdrConfig::validate`'s bandwidth check
    // silently passes NaN (comparisons against NaN are always false), and
    // the value then reaches `GapDetector::new`'s
    // `Duration::from_secs_f64`, which panics. Caught at CLI-parse time
    // instead, before any source is opened.
    // "-inf"/negative values aren't exercised here: clap treats a leading
    // "-" as a new flag rather than this value (a separate, pre-existing
    // parsing behavior, not part of the NaN-panic finding this test
    // covers) unless `allow_negative_numbers` is set, which this flag
    // deliberately doesn't need since every legitimate rate is positive.
    // "1e-20" covers the round-2 finding: finite and positive, but still
    // small enough to overflow `Duration::from_secs_f64` downstream.
    for bad_rate in ["NaN", "inf", "0", "1e-20"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([
                "listen",
                "--hpsdr-host",
                "192.168.1.100",
                "--hpsdr-freq",
                "14000000",
                "--hpsdr-rate",
                bad_rate,
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--hpsdr-rate {bad_rate} should be rejected before any I/O"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("hpsdr-rate"),
            "expected an explanatory error for --hpsdr-rate {bad_rate}, got: {stderr}"
        );
    }
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_freq_rejects_non_finite_and_non_positive_values() {
    // Round-2 review finding: --hpsdr-freq was never validated at all --
    // `HpsdrConfig` only length-checks `center_freq_hz`, not its values, so
    // NaN/inf/non-positive input propagated into every emitted spot's
    // frequency field. Matches `parse_dial_freq_hz`'s validation.
    for bad_freq in ["NaN", "inf", "-inf", "0", "-14000000"] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args([
                "listen",
                "--hpsdr-host",
                "192.168.1.100",
                "--hpsdr-freq",
                bad_freq,
                "--hpsdr-rate",
                "192000",
            ])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--hpsdr-freq {bad_freq} should be rejected before any I/O"
        );
    }
}

#[test]
#[cfg(feature = "hpsdr")]
fn hpsdr_host_conflicts_with_kiwi_host() {
    // Round-1 review finding: without this, clap accepted --hpsdr-* and
    // --kiwi-* together, opened the HPSDR source first, but reported the
    // Kiwi source name to the server's health metrics -- silently ignoring
    // the requested Kiwi source.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args([
            "listen",
            "--hpsdr-host",
            "192.168.1.100",
            "--hpsdr-freq",
            "14000000",
            "--hpsdr-rate",
            "192000",
            "--kiwi-host",
            "example.com",
            "--kiwi-freq",
            "14000000",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--hpsdr-host and --kiwi-host together should be a clean clap error"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "expected a clap conflict error, got: {stderr}"
    );
}

/// MAN-135 renamed the Hz-valued flags to a consistent `-hz` suffix and
/// kept the old spellings as hidden aliases. Anyone's existing scripts
/// and systemd units must keep working; this test is what says so.
/// A bogus port makes the run fail at connect, well after clap has
/// accepted (or rejected) the flag -- which is what we are testing.
#[test]
fn legacy_kiwi_freq_spelling_still_parses() {
    for spelling in ["--kiwi-freq-hz", "--kiwi-freq"] {
        let out = manta()
            .args([
                "soak",
                "--duration",
                "1",
                "--kiwi-host",
                "127.0.0.1",
                "--kiwi-port",
                "1",
                spelling,
                "7030000",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("unexpected argument"),
            "{spelling} should still be accepted, got: {stderr}"
        );
    }
}

#[test]
#[cfg(feature = "hpsdr")]
fn legacy_hpsdr_freq_and_rate_spellings_still_parse() {
    for spelling in ["--hpsdr-freq-hz", "--hpsdr-freq"] {
        let out = manta()
            .args([
                "soak",
                "--duration",
                "1",
                "--hpsdr-host",
                "127.0.0.1",
                "--hpsdr-port",
                "1",
                spelling,
                "14000000",
                "--hpsdr-rate",
                "192000",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("unexpected argument"),
            "{spelling} should still be accepted, got: {stderr}"
        );
    }
    for spelling in ["--hpsdr-rate-hz", "--hpsdr-rate"] {
        let out = manta()
            .args([
                "soak",
                "--duration",
                "1",
                "--hpsdr-host",
                "127.0.0.1",
                "--hpsdr-port",
                "1",
                "--hpsdr-freq-hz",
                "14000000",
                spelling,
                "192000",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("unexpected argument"),
            "{spelling} should still be accepted, got: {stderr}"
        );
    }
}

#[test]
#[cfg(feature = "soapy")]
fn legacy_soapy_freq_and_rate_spellings_still_parse() {
    for spelling in ["--soapy-freq-hz", "--soapy-freq"] {
        let out = manta()
            .args([
                "soak",
                "--duration",
                "1",
                "--soapy-driver",
                "driver=rtlsdr",
                spelling,
                "14000000",
                "--soapy-rate",
                "192000",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("unexpected argument"),
            "{spelling} should still be accepted, got: {stderr}"
        );
    }
    for spelling in ["--soapy-rate-hz", "--soapy-rate"] {
        let out = manta()
            .args([
                "soak",
                "--duration",
                "1",
                "--soapy-driver",
                "driver=rtlsdr",
                "--soapy-freq-hz",
                "14000000",
                spelling,
                "192000",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("unexpected argument"),
            "{spelling} should still be accepted, got: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// MAN-261: one TOML file drives a deployment. Phase 1 -- the strict loader;
// servers start iff [server] is present; scenario 3 (table) and scenario 4.
// ---------------------------------------------------------------------------

/// MAN-261 scenario 3 (table): a typo'd table is a hard error that names it,
/// raised before any source I/O. Before MAN-261, `[detectr]` was silently
/// ignored and the run failed only on the missing WAV (R2).
#[test]
fn unknown_table_in_config_fails_before_any_source_io() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        "manta.toml",
        &format!("{SERVER_TOML}[detectr]\non_snr_db = 99.0\n"),
    );
    for (what, mut cmd) in every_config_command(&cfg).into_iter().take(2) {
        let out = cmd.output().unwrap();
        assert_rejected_before_source_io(what, &out, &["detectr", "unrecognized"]);
    }
}

/// MAN-261 D7: `run --config` with no `[server]` table decodes without the
/// servers and says so. Before MAN-261 the same file failed late, after the
/// source opened, with `missing field `server`` (R9).
#[test]
fn config_without_server_table_runs_without_servers() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let cfg = write_cfg(
        dir.path(),
        "decode-only.toml",
        "[decode]\nengine = \"legacy\"\n",
    );

    let out = manta()
        .args(["run", "--json", "--source"])
        .arg(&wav)
        .args(["--source-iq", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    single_spot_freq(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no [server] table"), "stderr: {stderr}");
    // The startup banner names the bound sockets (`telnet=...`); none bound.
    assert!(!stderr.contains("telnet="), "servers started: {stderr}");
}

/// MAN-261 scenario 4, regression guard: a 0xFF byte in `run --source`'s
/// path is an ordinary error (exit 1), never a panic (exit 101). Green from
/// the start by design -- scenario 4 does not reproduce on `main`: the only
/// argv scan, `warn_deprecations`, already reads `args_os()` lossily, and
/// the panic MAN-74's validation found came from that branch's own
/// `std::env::args()` call. This pins the behaviour so the new config and
/// environment tiers cannot regress it.
#[test]
#[cfg(unix)]
fn run_with_a_non_utf8_source_path_reports_an_error_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let source = non_utf8_path(dir.path(), b"man261-\xff.wav");
    let out = manta()
        .arg("run")
        .arg("--source")
        .arg(&source)
        .output()
        .unwrap();
    assert_exit_1_without_panic(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("open WAV"), "stderr: {stderr}");
}

/// MAN-261 scenario 4, regression guard: a 0xFF byte in the `--config` path
/// is an ordinary error (exit 1), never a panic. Green from the start by
/// design: scenario 4 does not reproduce on `main` (see
/// `run_with_a_non_utf8_source_path_reports_an_error_instead_of_panicking`).
/// The config loader now reads this path first, so it is the code under
/// guard.
#[test]
#[cfg(unix)]
fn run_with_a_non_utf8_config_path_reports_an_error_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = non_utf8_path(dir.path(), b"man261-\xff.toml");
    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("missing.wav"))
        .args(["--dial-freq-hz", "14025000", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_exit_1_without_panic(&out);
}

/// MAN-261 scenario 4, regression guard: a non-UTF-8 value in an unrelated
/// (non-`MANTA_*`) environment variable is skipped, not inspected -- the
/// run fails only for the expected reason (the missing WAV), exit 1, no
/// panic. Green from the start by design: scenario 4 does not reproduce on
/// `main` (see `run_with_a_non_utf8_source_path_reports_an_error_instead_of_panicking`);
/// this guards the new environment tier's `vars_os()` read. The name
/// deliberately lacks the `MANTA_` prefix, which the strict tier would
/// reject on its own.
#[test]
#[cfg(unix)]
fn an_unrelated_non_utf8_env_var_does_not_panic_run() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = tempfile::tempdir().unwrap();
    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("missing.wav"))
        .env("ZZ_MAN261_NON_UTF8", std::ffi::OsStr::from_bytes(b"\xff"))
        .output()
        .unwrap();
    assert_exit_1_without_panic(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("open WAV"), "stderr: {stderr}");
}

// ---------------------------------------------------------------------------
// MAN-261 Phase 2: [detector] and [spot].
// ---------------------------------------------------------------------------

/// MAN-261: `[detector] on_snr_db` reaches `run`'s track manager. 99 dB
/// promotes no track, so V1's W1AW spot (present without the file) is gone.
#[test]
fn detector_on_snr_db_from_config_silences_run() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let cfg = write_cfg(dir.path(), "det.toml", "[detector]\non_snr_db = 99.0\n");

    let control = manta()
        .args(["run", "--json", "--source"])
        .arg(&wav)
        .arg("--source-iq")
        .output()
        .unwrap();
    single_spot_freq(&control);

    let out = manta()
        .args(["run", "--json", "--source"])
        .arg(&wav)
        .args(["--source-iq", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    let spots = run_spots(&out.stdout);
    assert!(
        spots.is_empty(),
        "on_snr_db = 99 must silence V1: {spots:?}"
    );
}

/// MAN-261: `[detector]` also reaches `decode`. With no track promoted,
/// `decode_samples` bails with `no signal found`.
#[test]
fn detector_on_snr_db_from_config_reaches_decode() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let cfg = write_cfg(dir.path(), "det.toml", "[detector]\non_snr_db = 99.0\n");

    let control = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(!decode_spots(&control).is_empty());

    let out = manta()
        .args(["decode", "--json", "--config"])
        .arg(&cfg)
        .arg(&wav)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no signal found"), "stderr: {stderr}");
}

/// MAN-261: `[spot] blocklist_path` suppresses a callsign, and a relative
/// path resolves against the config file's directory, not the CWD (D6).
/// Mirrors `decode_blocklist_flag_suppresses_a_callsign`.
#[test]
fn spot_blocklist_path_from_config_suppresses_a_callsign() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    std::fs::write(dir.path().join("bad-calls.txt"), "W1AW\n").unwrap();
    let cfg = write_cfg(
        dir.path(),
        "manta.toml",
        "[spot]\nblocklist_path = \"bad-calls.txt\"\n",
    );
    let is_w1aw = |s: &serde_json::Value| s["callsign"] == "W1AW";

    let control = manta()
        .current_dir(cwd.path())
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(decode_spots(&control).iter().any(is_w1aw));

    let out = manta()
        .current_dir(cwd.path())
        .args(["decode", "--json", "--config"])
        .arg(&cfg)
        .arg(&wav)
        .output()
        .unwrap();
    let spots = decode_spots(&out);
    assert!(
        !spots.iter().any(is_w1aw),
        "blocklisted W1AW must never be spotted: {spots:?}"
    );
}

// ---------------------------------------------------------------------------
// MAN-261 Phase 3: [input], CLI-over-file precedence, --config on soak/doctor.
// ---------------------------------------------------------------------------

/// Kills and reaps the child on drop, so a failed assertion never leaks a
/// `manta run` still attached to the fake KiwiSDR.
struct ChildGuard(std::process::Child);

impl ChildGuard {
    /// Kills the child (harmless if it already exited) and returns its stderr.
    fn finish(&mut self) -> String {
        use std::io::Read as _;
        let _ = self.0.kill();
        let _ = self.0.wait();
        let mut stderr = String::new();
        if let Some(mut pipe) = self.0.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        stderr
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the fake KiwiSDR saw of the client's handshake.
struct KiwiSession {
    /// The WebSocket request path (`/<timestamp>/SND`).
    path: String,
    /// The first text frame (`SET auth t=kiwi p=<password>`).
    auth: String,
    /// The tuning command (`SET mod=iq ... freq=<kHz>`).
    tune: String,
}

/// Records the WebSocket upgrade request's path for [`fake_kiwi_session`].
/// A trait impl rather than a closure: `Callback`'s large `Err` type is
/// tungstenite's to choose, and clippy only flags it on closures.
struct CapturePath<'a>(&'a mut String);

impl tungstenite::handshake::server::Callback for CapturePath<'_> {
    fn on_request(
        self,
        request: &tungstenite::handshake::server::Request,
        response: tungstenite::handshake::server::Response,
    ) -> Result<
        tungstenite::handshake::server::Response,
        tungstenite::handshake::server::ErrorResponse,
    > {
        *self.0 = request.uri().path().to_owned();
        Ok(response)
    }
}

/// Plays a KiwiSDR's side of `KiwiIqSource::connect`
/// (crates/manta-input/src/kiwi.rs): accept the WebSocket on `/<ts>/SND`,
/// read `SET auth`, report `sample_rate=` in a binary `MSG` frame, then read
/// until the `SET mod=iq` tuning command. Every wait is bounded (20 s to
/// connect, 10 s per read) so a regression fails instead of hanging, and a
/// child that exits without connecting fails at once.
fn fake_kiwi_session(
    listener: &std::net::TcpListener,
    child: &mut std::process::Child,
) -> Result<KiwiSession, String> {
    use std::time::{Duration, Instant};
    use tungstenite::Message;

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    return Err(format!(
                        "manta exited ({status}) without connecting to the fake KiwiSDR"
                    ));
                }
                if Instant::now() >= deadline {
                    return Err("no connection to the fake KiwiSDR within 20 s".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("accept: {e}")),
        }
    };
    // BSD/macOS sockets inherit the listener's non-blocking mode.
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    let timeout = Some(Duration::from_secs(10));
    stream
        .set_read_timeout(timeout)
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| e.to_string())?;

    let mut path = String::new();
    let mut ws = tungstenite::accept_hdr(stream, CapturePath(&mut path))
        .map_err(|e| format!("WebSocket handshake: {e}"))?;

    let auth = match ws.read().map_err(|e| format!("reading SET auth: {e}"))? {
        Message::Text(t) => t.as_str().to_owned(),
        other => return Err(format!("expected SET auth first, got {other:?}")),
    };
    // 12000 Hz / 93.75 Hz per channel = 128 channels: a valid rate.
    ws.send(Message::binary(b"MSG sample_rate=12000.000".to_vec()))
        .map_err(|e| format!("sending sample_rate: {e}"))?;
    for _ in 0..32 {
        match ws
            .read()
            .map_err(|e| format!("reading SET commands: {e}"))?
        {
            Message::Text(t) if t.as_str().starts_with("SET mod=iq") => {
                let tune = t.as_str().to_owned();
                return Ok(KiwiSession { path, auth, tune });
            }
            _ => continue,
        }
    }
    Err("no SET mod=iq within 32 frames".into())
}

/// MAN-261 scenario 1: an `[input] type = "kiwi"` table alone makes
/// `manta run --config f.toml` (no source flags) connect to that KiwiSDR,
/// authenticate with its password and tune `freq_hz`. Hermetic: the KiwiSDR
/// is an in-process fake on 127.0.0.1:0. Before MAN-261 `[input]` was never
/// consulted and the run fell through to the default audio device (R1).
#[test]
fn kiwi_input_table_opens_that_source_with_no_source_flags() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        "kiwi.toml",
        &format!(
            "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = {port}\n\
             freq_hz = 14025000.0\npassword = \"pw\"\n"
        ),
    );

    let mut child = ChildGuard(
        manta()
            .args(["run", "--config"])
            .arg(&cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let session = fake_kiwi_session(&listener, &mut child.0);
    let stderr = child.finish();
    let session = session.unwrap_or_else(|e| panic!("{e}; manta stderr: {stderr}"));
    assert!(session.path.ends_with("/SND"), "path: {}", session.path);
    assert_eq!(session.auth, "SET auth t=kiwi p=pw");
    assert!(
        session.tune.contains("freq=14025.000"),
        "tune: {}",
        session.tune
    );
}

/// The scenario-2 fixture: [`v1_fixture`] in `dir` plus `dir/manta.toml`,
/// whose `[input]` names the WAV relative to the config's own directory and
/// sets `freq_correction_ppm = 2.5`. Returns `(wav, cfg)`.
fn ppm_fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let wav = v1_fixture(dir);
    let cfg = write_cfg(
        dir,
        "manta.toml",
        "[input]\ntype = \"file\"\npath = \"v1.wav\"\niq = true\nfreq_correction_ppm = 2.5\n",
    );
    (wav, cfg)
}

/// `run --json --source <wav> --source-iq` from `cwd`, no config: the
/// uncorrected baseline spot frequency.
fn baseline_spot_freq(cwd: &Path, wav: &Path) -> f64 {
    let out = manta()
        .current_dir(cwd)
        .args(["run", "--json", "--source"])
        .arg(wav)
        .arg("--source-iq")
        .output()
        .unwrap();
    single_spot_freq(&out)
}

/// MAN-261 scenario 2: `[input] freq_correction_ppm` scales the spot
/// frequency exactly as the flag does, and an explicit
/// `--freq-correction-ppm` beats it. Runs from a different CWD so the file's
/// relative `path` must resolve against the config's directory. Before
/// MAN-261 the file's ppm was ignored (R6).
#[test]
fn config_freq_correction_ppm_applies_and_the_cli_flag_overrides_it() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (wav, cfg) = ppm_fixture(dir.path());
    let base = baseline_spot_freq(cwd.path(), &wav);

    let from_file = manta()
        .current_dir(cwd.path())
        .args(["run", "--json", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert_ppm_scaled(single_spot_freq(&from_file), base, 2.5, "file ppm 2.5");

    let from_flag = manta()
        .current_dir(cwd.path())
        .args(["run", "--json", "--config"])
        .arg(&cfg)
        .args(["--freq-correction-ppm", "4.0"])
        .output()
        .unwrap();
    assert_ppm_scaled(
        single_spot_freq(&from_flag),
        base,
        4.0,
        "--freq-correction-ppm 4.0 over the file's 2.5",
    );
}

/// MAN-261 scenario 3 (ppm): an out-of-range `[input] freq_correction_ppm`
/// fails with the flag validator's own message, on every command that takes
/// `--config`, before any source I/O. Before MAN-261 `decode` loaded it and
/// failed only on the missing WAV (R3).
#[test]
fn out_of_range_freq_correction_ppm_in_input_is_rejected_like_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        "ppm.toml",
        "[input]\nfreq_correction_ppm = 999999\n",
    );
    for (what, mut cmd) in every_config_command(&cfg) {
        let out = cmd.output().unwrap();
        assert_rejected_before_source_io(
            what,
            &out,
            &["freq_correction_ppm 999999 is outside the supported range [-1000, 1000]"],
        );
    }
}

/// MAN-261 scenario 3 (table) on the two commands that gained `--config`.
#[test]
fn unknown_table_in_config_fails_for_soak_and_doctor() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        "manta.toml",
        &format!("{SERVER_TOML}[detectr]\non_snr_db = 99.0\n"),
    );
    for (what, mut cmd) in every_config_command(&cfg).into_iter().skip(2) {
        let out = cmd.output().unwrap();
        assert_rejected_before_source_io(what, &out, &["detectr", "unrecognized"]);
    }
}

/// MAN-261 D6: a CLI source flag defines the whole source. A typed `[input]`
/// table is discarded as a unit -- including its shared
/// `freq_correction_ppm` -- with a note. Nothing listens on port 1, so any
/// attempt to use the KiwiSDR table would fail the run.
#[test]
fn cli_source_flag_replaces_a_typed_input_table() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let cfg = write_cfg(
        dir.path(),
        "kiwi.toml",
        "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = 1\nfreq_hz = 14025000.0\n\
         freq_correction_ppm = 2.5\n",
    );
    let base = baseline_spot_freq(dir.path(), &wav);

    let out = manta()
        .args(["run", "--json", "--source"])
        .arg(&wav)
        .args(["--source-iq", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let freq = single_spot_freq(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ignoring [input]"), "stderr: {stderr}");
    assert_ppm_scaled(freq, base, 0.0, "the discarded table's ppm must not apply");
}

/// MAN-261 D6: `--source-iq` without `--source` sets `iq = true` on a config
/// `type = "file"` source. Without it, V1's 96 kHz IQ WAV goes through
/// `AudioIqSource`, which accepts only 48000 Hz.
#[test]
fn source_iq_flag_applies_to_a_config_file_source() {
    let dir = tempfile::tempdir().unwrap();
    v1_fixture(dir.path());
    let cfg = write_cfg(
        dir.path(),
        "file.toml",
        "[input]\ntype = \"file\"\npath = \"v1.wav\"\n",
    );

    let out = manta()
        .args(["run", "--json", "--source-iq", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    single_spot_freq(&out);

    let without = manta()
        .args(["run", "--json", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!without.status.success());
    let stderr = String::from_utf8_lossy(&without.stderr);
    assert!(stderr.contains("48000"), "stderr: {stderr}");
}

/// MAN-261 D7: `input.center_freq_hz` satisfies `run`'s dial guard and takes
/// exactly `--dial-freq-hz`'s path. The fixture's sidecar reports the zero
/// "unknown center" sentinel, so only the operator's dial makes it RF-aware.
#[test]
fn config_center_freq_hz_satisfies_the_dial_guard() {
    let dir = tempfile::tempdir().unwrap();
    let wav = write_fixture(
        dir.path(),
        &manta_testkit::vectors::VectorSpec {
            center_freq_hz: 0.0,
            ..short_v1()
        },
    );
    let file_input = "[input]\ntype = \"file\"\npath = \"v1.wav\"\niq = true\n";
    let server_only = write_cfg(dir.path(), "server.toml", SERVER_TOML);
    let with_center = write_cfg(
        dir.path(),
        "center.toml",
        &format!("{SERVER_TOML}{file_input}center_freq_hz = 14000000.0\n"),
    );
    let without_center = write_cfg(
        dir.path(),
        "no-center.toml",
        &format!("{SERVER_TOML}{file_input}"),
    );

    let from_file = manta()
        .args(["run", "--json", "--config"])
        .arg(&with_center)
        .output()
        .unwrap();
    let from_file = single_spot_freq(&from_file);
    let from_flag = manta()
        .args(["run", "--json", "--source"])
        .arg(&wav)
        .args(["--source-iq", "--dial-freq-hz", "14000000", "--config"])
        .arg(&server_only)
        .output()
        .unwrap();
    let from_flag = single_spot_freq(&from_flag);
    assert!(from_file > 14_000_000.0, "not absolute: {from_file}");
    assert!(
        (from_file - from_flag).abs() < 1e-3,
        "input.center_freq_hz gave {from_file}, --dial-freq-hz gave {from_flag}"
    );

    let out = manta()
        .args(["run", "--json", "--config"])
        .arg(&without_center)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(DIAL_GUARD), "stderr: {stderr}");
}

/// MAN-261: `doctor --config` reads `[input]`, including `center_freq_hz`.
/// Pattern: `doctor_accepts_and_applies_dial_freq_hz_for_an_audio_source`.
#[test]
fn doctor_reports_center_freq_hz_from_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    v1_fixture(dir.path());
    let cfg = write_cfg(
        dir.path(),
        "doctor.toml",
        "[input]\ntype = \"file\"\npath = \"v1.wav\"\niq = true\ncenter_freq_hz = 7030000.0\n",
    );

    let out = manta()
        .args(["doctor", "--duration", "3", "--json", "--config"])
        .arg(&cfg)
        .arg("--ntp-server")
        .arg(closed_ntp_server())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("doctor --json not JSON ({e}): stdout={stdout} stderr={stderr}")
    });
    assert_eq!(
        report["center_freq_hz"].as_f64(),
        Some(7_030_000.0),
        "report: {report}"
    );
}

/// MAN-261: `soak --config` opens the `[input]` source with no source flags.
#[test]
fn soak_runs_from_a_config_file_source() {
    let dir = tempfile::tempdir().unwrap();
    v1_fixture(dir.path());
    let cfg = write_cfg(
        dir.path(),
        "soak.toml",
        "[input]\ntype = \"file\"\npath = \"v1.wav\"\niq = true\n",
    );

    let out = manta()
        .args(["soak", "--duration", "2", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("SoakReport"), "stderr: {stderr}");
}

/// MAN-261 D7: `soak` never starts the spot servers; a `[server]` table in
/// its config earns a note instead. MAN-126 D10
/// (docs/DECISIONS/2026-10-10-man126-doctor-setup-checks.md): `doctor` now
/// checks `[server]`'s ports instead of ignoring the table, still starting
/// no server; every port here is 0, so each port row is a SKIP.
#[test]
fn soak_notes_an_ignored_server_table_and_doctor_checks_its_ports() {
    let dir = tempfile::tempdir().unwrap();
    v1_fixture(dir.path());
    let cfg = write_cfg(
        dir.path(),
        "manta.toml",
        &format!(
            "{SERVER_TOML}[input]\ntype = \"file\"\npath = \"v1.wav\"\niq = true\n\
             center_freq_hz = 7030000.0\n"
        ),
    );

    let out = manta()
        .args(["soak", "--duration", "2", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "soak: stderr: {stderr}");
    assert!(
        stderr.contains("does not start the spot servers"),
        "soak: stderr: {stderr}"
    );
    assert!(!stderr.contains("telnet="), "soak: {stderr}");

    let out = manta()
        .args(["doctor", "--duration", "3", "--json", "--config"])
        .arg(&cfg)
        .arg("--ntp-server")
        .arg(closed_ntp_server())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "doctor: stdout: {stdout} stderr: {stderr}"
    );
    assert!(
        !stderr.contains("does not start the spot servers"),
        "doctor: stderr: {stderr}"
    );
    assert!(!stderr.contains("telnet="), "doctor: {stderr}");
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let checks = report["checks"].as_array().unwrap();
    for name in ["telnet port", "json port", "metrics port"] {
        let row = checks
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no {name} row in {report}"));
        assert_eq!(row["status"], "skip", "{row}");
    }
}

// ---------------------------------------------------------------------------
// MAN-261 Phase 4: the MANTA_* environment tier (run, soak, doctor only).
// ---------------------------------------------------------------------------

/// MAN-261 precedence: CLI flag > `MANTA_*` > file > default, shown on
/// `freq_correction_ppm` (file 2.5, env 3.0, flag 4.0).
#[test]
fn env_ppm_sits_between_file_and_cli() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (wav, cfg) = ppm_fixture(dir.path());
    let base = baseline_spot_freq(cwd.path(), &wav);

    let from_env = manta()
        .current_dir(cwd.path())
        .args(["run", "--json", "--config"])
        .arg(&cfg)
        .env("MANTA_INPUT_FREQ_CORRECTION_PPM", "3.0")
        .output()
        .unwrap();
    assert_ppm_scaled(
        single_spot_freq(&from_env),
        base,
        3.0,
        "MANTA_INPUT_FREQ_CORRECTION_PPM=3.0 over the file's 2.5",
    );

    let from_flag = manta()
        .current_dir(cwd.path())
        .args(["run", "--json", "--config"])
        .arg(&cfg)
        .args(["--freq-correction-ppm", "4.0"])
        .env("MANTA_INPUT_FREQ_CORRECTION_PPM", "3.0")
        .output()
        .unwrap();
    assert_ppm_scaled(
        single_spot_freq(&from_flag),
        base,
        4.0,
        "--freq-correction-ppm 4.0 over MANTA_INPUT_FREQ_CORRECTION_PPM=3.0",
    );
}

/// MAN-261 D8: `MANTA_CONFIG` names the config when `--config` is absent, and
/// an explicit `--config` beats it.
#[test]
fn manta_config_env_is_the_config_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    std::fs::write(dir.path().join("bad-calls.txt"), "W1AW\n").unwrap();
    let block = write_cfg(
        dir.path(),
        "block.toml",
        "[spot]\nblocklist_path = \"bad-calls.txt\"\n",
    );
    let other = write_cfg(dir.path(), "other.toml", "[decode]\nengine = \"legacy\"\n");
    let is_w1aw = |s: &serde_json::Value| s["callsign"] == "W1AW";

    let from_env = manta()
        .args(["run", "--json", "--source"])
        .arg(&wav)
        .arg("--source-iq")
        .env("MANTA_CONFIG", &block)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&from_env.stderr);
    assert!(from_env.status.success(), "stderr: {stderr}");
    let spots = run_spots(&from_env.stdout);
    assert!(
        !spots.iter().any(is_w1aw),
        "MANTA_CONFIG's blocklist must apply: {spots:?}"
    );

    let from_flag = manta()
        .args(["run", "--json", "--source"])
        .arg(&wav)
        .args(["--source-iq", "--config"])
        .arg(&other)
        .env("MANTA_CONFIG", &block)
        .output()
        .unwrap();
    single_spot_freq(&from_flag);
    assert!(
        run_spots(&from_flag.stdout).iter().any(is_w1aw),
        "--config must beat MANTA_CONFIG"
    );
}

/// MAN-261 scenario 4: a non-UTF-8 value in a `MANTA_*` variable is an error
/// that names the variable (exit 1), never a panic, and it is raised before
/// any source I/O.
#[test]
#[cfg(unix)]
fn non_utf8_manta_env_value_is_an_error_not_a_panic() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = tempfile::tempdir().unwrap();
    let out = manta()
        .args(["run", "--source"])
        .arg(dir.path().join("missing.wav"))
        .arg("--source-iq")
        .env("MANTA_INPUT_HOST", std::ffi::OsStr::from_bytes(b"h\xff"))
        .output()
        .unwrap();
    assert_exit_1_without_panic(&out);
    assert_rejected_before_source_io("run", &out, &["MANTA_INPUT_HOST is not valid UTF-8"]);
}

/// MAN-261 D8: `decode` is the deterministic golden-vector tool and never
/// reads the environment -- not `MANTA_<TABLE>_<KEY>`, not unknown `MANTA_*`
/// names, not `MANTA_CONFIG` -- so its output is byte-identical with and
/// without them.
#[test]
fn decode_ignores_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let det = write_cfg(dir.path(), "det.toml", "[detector]\non_snr_db = 99.0\n");

    let plain = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert_eq!(decode_spots(&plain).len(), 1);

    let with_env = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .env("MANTA_DETECTOR_ON_SNR_DB", "99")
        .env("MANTA_FOO", "1")
        .env("MANTA_CONFIG", &det)
        .output()
        .unwrap();
    assert_eq!(decode_spots(&with_env).len(), 1);
    assert_eq!(plain.stdout, with_env.stdout);
}

/// MAN-44: `manta status` against an address nothing is listening on must
/// fail cleanly with exit code 2 ("couldn't ask", distinct from exit 1
/// "asked, unhealthy") and a plain, non-panicking message naming the
/// address -- not hang, and not a backtrace.
///
/// CI remediate finding: the previous version bound a listener, dropped it
/// immediately, then spawned the `manta` binary as a SEPARATE PROCESS to
/// connect to the now-freed port -- unlike every other "nothing is
/// listening" test in this workspace (e.g. `uplink.rs`'s
/// `connect_first_reachable_errors_when_every_address_fails`), which
/// reconnect in-process microseconds after the drop, process spawn (fork/
/// exec, dynamic-linker startup, tokio runtime init) can easily take tens
/// of milliseconds -- long enough for an unrelated concurrently-running
/// test elsewhere in this same `cargo test` invocation to bind that exact
/// ephemeral port before this subprocess ever dials it, especially on a
/// host with a narrower ephemeral port range (observed: this was flaky
/// specifically on `macos-latest`, never on `ubuntu-latest`, across
/// `test`/`test-soapy`/`test-hpsdr` -- all three share this file, none
/// share a root cause that would be feature- or OS-conditional otherwise).
/// Keeping the listener alive but never calling `.accept()` on it removes
/// the race entirely: the kernel completes the TCP handshake into the
/// unconsumed backlog, so `manta status` connects successfully, then
/// blocks reading a response that will never come, and reliably fails via
/// `--timeout-secs` instead -- exit code 2 and an address-naming message
/// either way, per `fetch_status`'s `timed out talking to {addr}` path.
#[test]
fn status_against_a_dead_address_fails_with_exit_code_two_and_a_plain_message() {
    // Bound but never `.accept()`-ed: connects fine at the TCP level, then
    // nothing ever answers, so `manta status` times out rather than racing
    // a freed port against unrelated concurrent tests (see doc comment).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let out = manta()
        .args(["status", "--addr", &addr.to_string(), "--timeout-secs", "2"])
        .output()
        .unwrap();
    drop(listener);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&addr.to_string()),
        "expected the dead address in the error message, got: {stderr}"
    );
}

/// MAN-44 CR-A regression: an invalid `--addr` must exit 2 ("couldn't
/// ask"), not 1 -- exit 1 is the documented "reached the daemon, uplink is
/// unhealthy" code (`docs/RUNBOOKS/uplink-health.md`), and a typo'd
/// address must not be indistinguishable from a genuinely degraded uplink.
#[test]
fn status_with_an_invalid_addr_fails_with_exit_code_two_not_one() {
    let out = manta()
        .args(["status", "--addr", "not-an-addr"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// MAN-44 CR-A regression: an unreadable `--server-config` must exit 2, not
/// 1, for the same reason -- this used to `?`-propagate out of `main` and
/// exit 1, colliding with the "unhealthy uplink" code.
///
/// Both spellings are exercised: `--server-config` is the deprecated
/// alias operators already have in their runbooks and cron jobs, and
/// `--config` is the canonical flag `warn_deprecations` tells them to
/// switch to. Before the Codex review fix (PR #95) the second one did not
/// exist on `status`, so the deprecation notice named a flag clap
/// rejected -- and this test would have failed with clap's exit 2-shaped
/// usage error for the wrong reason, hence the message assertion.
#[test]
fn status_with_a_missing_server_config_fails_with_exit_code_two_not_one() {
    for spelling in ["--server-config", "--config"] {
        let out = manta()
            .args(["status", spelling, "/nonexistent/manta.toml"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "`manta status {spelling}` stderr: {stderr}"
        );
        assert!(
            stderr.contains("manta status:") && stderr.contains("/nonexistent/manta.toml"),
            "`manta status {spelling}` must fail reading the config, not on clap usage: {stderr}"
        );
    }
}

// MAN-79: the decoded call is valid only in the operator's updated table.
fn qq9_fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let mut spec = short_v1();
    spec.signals[0].text = "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into();
    let wav = write_fixture(dir, &spec);
    let cty = write_cfg(
        dir,
        "cty-new.dat",
        &format!(
            "{}Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n",
            manta_spot::CTY_DAT
        ),
    );
    (wav, cty)
}

#[test]
fn decode_cty_flag_lets_a_newly_allocated_prefix_spot() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, cty) = qq9_fixture(dir.path());
    let before = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(decode_spots(&before).is_empty());
    let after = manta()
        .args(["decode", "--json", "--cty"])
        .arg(cty)
        .arg(wav)
        .output()
        .unwrap();
    let spots = decode_spots(&after);
    assert_eq!(spots.len(), 1);
    assert_eq!(spots[0]["callsign"], "QQ9ZZZ");
}

#[test]
fn decode_cty_path_in_a_config_file_lets_a_newly_allocated_prefix_spot() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("config");
    std::fs::create_dir(&sub).unwrap();
    let (wav, _) = qq9_fixture(&sub);
    let config = write_cfg(&sub, "manta.toml", "[spot]\ncty_path = 'cty-new.dat'\n");
    let out = manta()
        .current_dir(dir.path())
        .args(["decode", "--json", "--config"])
        .arg(config)
        .arg(wav)
        .output()
        .unwrap();
    let spots = decode_spots(&out);
    assert_eq!(spots.len(), 1);
    assert_eq!(spots[0]["callsign"], "QQ9ZZZ");
}

#[test]
fn run_reads_cty_path_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, cty) = qq9_fixture(dir.path());
    let out = manta()
        .args(["run", "--json", "--source-iq", "--source"])
        .arg(wav)
        .env("MANTA_SPOT_CTY_PATH", cty)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(run_spots(&out.stdout)
        .iter()
        .any(|s| s["callsign"] == "QQ9ZZZ"));
}

#[test]
fn decode_scp_flag_is_accepted_and_still_spots() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let scp = write_cfg(dir.path(), "MASTER.SCP", "W1AW\n");
    let out = manta()
        .args(["decode", "--json", "--scp"])
        .arg(scp)
        .arg(wav)
        .output()
        .unwrap();
    assert!(decode_spots(&out).iter().any(|s| s["callsign"] == "W1AW"));
}

#[test]
fn a_cty_file_with_no_prefixes_is_rejected_before_source_io() {
    let dir = tempfile::tempdir().unwrap();
    let cty = write_cfg(
        dir.path(),
        "cty.csv",
        "1A,Sov Mil Order of Malta,246,EU,15,28,41.90,-12.43,-1.0,1A;\n",
    );
    let out = manta()
        .args(["run", "--source", "does-not-exist.wav", "--cty"])
        .arg(cty)
        .output()
        .unwrap();
    assert_rejected_before_source_io("cty.csv", &out, &["cty.csv", "lists no callsign prefixes"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn decode_never_prints_the_cty_age_warning() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture(dir.path());
    let out = manta()
        .args(["decode", "--json"])
        .arg(wav)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("built-in cty.dat"));
}
