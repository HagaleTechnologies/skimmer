//! MAN-124 acceptance: `manta run`'s stderr log is plain text when
//! redirected, its level follows `-v`/`-q`/`--log-level` (shorthand for
//! `RUST_LOG`), and `--log-format json` makes every stderr line a JSON
//! record. The harness pipes stderr, so it is never a terminal here.
use std::path::{Path, PathBuf};
use std::process::Output;

/// V1 shortened to 30 s: long enough for its one W1AW CQ spot (see
/// `cli.rs`'s `short_v1`).
fn short_v1() -> manta_testkit::vectors::VectorSpec {
    manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    }
}

/// Writes `spec`'s fixture set into `dir` and returns the WAV's path.
fn write_fixture(dir: &Path, spec: &manta_testkit::vectors::VectorSpec) -> PathBuf {
    manta_testkit::vectors::write_fixture_set(spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

const SERVER: &str = r#"
[server]
station_callsign = "W3XYZ"
bind_addr = "127.0.0.1"
telnet_port = 0
json_port = 0
metrics_port = 0
"#;

/// A loopback `[server]` config on ephemeral ports, plus `extra` TOML.
fn server_config(dir: &Path, extra: &str) -> PathBuf {
    let path = dir.join("manta.toml");
    std::fs::write(&path, format!("{SERVER}{extra}")).unwrap();
    path
}

/// `manta run` on the short V1 fixture with `cfg`, `extra_args` appended,
/// `NO_COLOR` and `RUST_LOG` removed, then `envs` applied. `--cty` pins the
/// table so the date-dependent age warning never appears.
fn daemon_with(dir: &Path, cfg: &Path, extra_args: &[&str], envs: &[(&str, &str)]) -> Output {
    let wav = dir.join("v1.wav");
    if !wav.exists() {
        write_fixture(dir, &short_v1());
    }
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_manta"));
    cmd.args([
        "run",
        "--cty",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../manta-spot/data/cty.dat"),
        "--source",
        wav.to_str().unwrap(),
        "--source-iq",
        "--dial-freq-hz",
        "14000000",
        "--config",
        cfg.to_str().unwrap(),
    ])
    .args(extra_args)
    .env_remove("NO_COLOR")
    .env_remove("RUST_LOG");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn daemon(dir: &Path, extra_args: &[&str], envs: &[(&str, &str)]) -> Output {
    let cfg = server_config(dir, "");
    daemon_with(dir, &cfg, extra_args, envs)
}

fn stderr_of(out: &Output) -> String {
    assert!(
        out.status.success(),
        "exit {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stderr.clone()).unwrap()
}

const READY: &str = "manta: listening; send SIGINT or SIGTERM to stop";
const GREETING_WARN: &str = " WARN manta: telnet greeting will omit QTH/grid";

/// The banner line: `INFO manta: manta <version> listening: ...`.
fn has_banner(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|l| l.contains(" INFO manta: manta ") && l.contains("listening:"))
}

/// Every stderr line, parsed; panics on any line that is not a JSON object
/// carrying string `timestamp`, `level`, `message` and `target`.
fn json_records(stderr: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let v: serde_json::Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l:?}"));
            for key in ["timestamp", "level", "message", "target"] {
                assert!(v[key].is_string(), "no string `{key}`: {l}");
            }
            v
        })
        .collect()
}

#[test]
fn a_redirected_daemon_log_has_no_escape_codes() {
    let dir = tempfile::tempdir().unwrap();
    // NO_COLOR is removed, so it is the terminal check, not NO_COLOR, that
    // turns colour off.
    let out = daemon(dir.path(), &[], &[("RUST_LOG", "info")]);
    let stderr = stderr_of(&out);
    assert!(!out.stderr.contains(&0x1b), "{stderr}");
    assert!(has_banner(&stderr), "no plain banner line: {stderr}");
}

#[test]
fn quiet_keeps_warnings_and_drops_info() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = stderr_of(&daemon(dir.path(), &["-q"], &[]));
    assert!(stderr.contains(GREETING_WARN), "{stderr}");
    assert!(!stderr.contains(" INFO "), "{stderr}");
    assert!(stderr.contains(READY), "{stderr}");
}

#[test]
fn log_level_warn_is_the_same_as_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = stderr_of(&daemon(dir.path(), &["--log-level", "warn"], &[]));
    assert!(stderr.contains(GREETING_WARN), "{stderr}");
    assert!(!stderr.contains(" INFO "), "{stderr}");
    assert!(stderr.contains(READY), "{stderr}");
}

#[test]
fn double_quiet_keeps_errors_only() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = stderr_of(&daemon(dir.path(), &["-qq"], &[]));
    assert!(!stderr.contains(" WARN "), "{stderr}");
    assert!(!stderr.contains(" INFO "), "{stderr}");
    assert!(stderr.contains(READY), "{stderr}");
}

#[test]
fn verbose_shows_debug_records() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = stderr_of(&daemon(dir.path(), &["-v"], &[]));
    assert!(
        stderr
            .lines()
            .any(|l| l.contains(" DEBUG manta: log filter") && l.contains("filter=debug")),
        "{stderr}"
    );
}

#[test]
fn default_is_info() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = stderr_of(&daemon(dir.path(), &[], &[]));
    assert!(has_banner(&stderr), "{stderr}");
    assert!(!stderr.contains(" DEBUG "), "{stderr}");
}

#[test]
fn rust_log_still_works_and_flags_override_it() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = stderr_of(&daemon(dir.path(), &[], &[("RUST_LOG", "warn")]));
    assert!(
        !stderr.contains("listening:"),
        "RUST_LOG=warn kept INFO: {stderr}"
    );
    let stderr = stderr_of(&daemon(
        dir.path(),
        &["--log-level", "info"],
        &[("RUST_LOG", "warn")],
    ));
    assert!(
        has_banner(&stderr),
        "--log-level did not beat RUST_LOG: {stderr}"
    );
}

#[test]
fn json_format_makes_every_stderr_line_a_json_object() {
    let dir = tempfile::tempdir().unwrap();
    let out = daemon(
        dir.path(),
        &["--log-format", "json"],
        &[("RUST_LOG", "info")],
    );
    let stderr = stderr_of(&out);
    assert!(!out.stderr.contains(&0x1b), "{stderr}");
    let records = json_records(&stderr);
    let message = |v: &serde_json::Value| v["message"].as_str().unwrap().to_owned();
    assert!(
        records.iter().any(|v| message(v).contains("listening:")),
        "no banner record: {stderr}"
    );
    assert!(
        records
            .iter()
            .any(|v| v["level"] == "INFO" && message(v) == READY),
        "no readiness record: {stderr}"
    );
    assert!(
        records
            .iter()
            .any(|v| v["level"] == "WARN"
                && message(v).contains("telnet greeting will omit QTH/grid")),
        "no greeting warning record: {stderr}"
    );
    // MAN-123: spots are stdout product output in either log format.
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.lines().any(|line| line.starts_with("SPOT: W1AW ")),
        "{stdout}"
    );
}

#[test]
fn json_format_carries_startup_notes() {
    let dir = tempfile::tempdir().unwrap();
    // `--source` overrides this `[input]`, which prints a startup note.
    let cfg = server_config(
        dir.path(),
        "[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7000000.0\n",
    );
    let stderr = stderr_of(&daemon_with(
        dir.path(),
        &cfg,
        &["--log-format", "json"],
        &[],
    ));
    let records = json_records(&stderr);
    assert!(
        records.iter().any(|v| v["level"] == "INFO"
            && v["message"]
                .as_str()
                .unwrap()
                .starts_with("note: --source selects the source")),
        "no startup-note record: {stderr}"
    );
}

#[test]
fn json_format_leaves_stdout_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    for json in [&["--json"][..], &[]] {
        let text = daemon(dir.path(), &[json, &["--log-format", "text"]].concat(), &[]);
        let structured = daemon(dir.path(), &[json, &["--log-format", "json"]].concat(), &[]);
        stderr_of(&text);
        stderr_of(&structured);
        assert!(!text.stdout.is_empty(), "{json:?}: empty stdout");
        assert!(
            text.stdout == structured.stdout,
            "{json:?}: stdout differs between log formats"
        );
    }
}

const BAD_CONFIG: &str = "[server]\nstation_callsign = \"W3XYZ\"\nbogus_key = 1\n";

#[test]
fn json_format_reports_a_fatal_error_as_one_record() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("bad.toml");
    std::fs::write(&cfg, BAD_CONFIG).unwrap();
    let out = daemon_with(dir.path(), &cfg, &["--log-format", "json"], &[]);
    let stderr = String::from_utf8(out.stderr.clone()).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let records = json_records(&stderr);
    assert_eq!(records.len(), 1, "{stderr}");
    let message = records[0]["message"].as_str().unwrap();
    assert_eq!(records[0]["level"], "ERROR", "{stderr}");
    assert!(message.contains("unknown field `bogus_key`"), "{stderr}");
    assert!(!message.ends_with('\n'), "{message:?}");
}

#[test]
fn text_format_fatal_error_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("bad.toml");
    std::fs::write(&cfg, BAD_CONFIG).unwrap();
    let out = daemon_with(dir.path(), &cfg, &[], &[]);
    let stderr = String::from_utf8(out.stderr.clone()).unwrap();
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.starts_with("Error: "), "{stderr}");
    assert!(stderr.contains("bogus_key"), "{stderr}");
}

#[test]
fn json_format_fatal_error_survives_a_filter_that_drops_it() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("bad.toml");
    std::fs::write(&cfg, BAD_CONFIG).unwrap();
    for (args, envs) in [
        (&["--log-format", "json", "--log-level", "off"][..], &[][..]),
        (&["--log-format", "json", "-qqq"][..], &[][..]),
        (
            &["--log-format", "json"][..],
            &[("RUST_LOG", "manta_server=debug")][..],
        ),
    ] {
        let out = daemon_with(dir.path(), &cfg, args, envs);
        let stderr = String::from_utf8(out.stderr.clone()).unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?} {envs:?}: {stderr}");
        // Still JSON, not Rust's plain `Error: …` line.
        let records = json_records(&stderr);
        assert_eq!(records.len(), 1, "{args:?} {envs:?}: {stderr}");
        assert_eq!(records[0]["level"], "ERROR", "{args:?} {envs:?}: {stderr}");
        assert!(
            records[0]["message"]
                .as_str()
                .unwrap()
                .contains("unknown field `bogus_key`"),
            "{args:?} {envs:?}: {stderr}"
        );
    }
}

#[test]
fn log_help_documents_unfiltered_text_helpers() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(
        help.contains("Text notes, warnings, readiness and reconnect lines are unfiltered"),
        "{help}"
    );
}

#[test]
fn json_format_carries_grouped_decoded_text() {
    let dir = tempfile::tempdir().unwrap();
    let out = daemon(dir.path(), &["--decoded-text", "--log-format", "json"], &[]);
    let stderr = stderr_of(&out);
    let records = json_records(&stderr);
    assert!(
        records.iter().any(|v| v["target"] == "manta::text"
            && v["level"] == "INFO"
            && v["message"].as_str().unwrap().starts_with("[track ")),
        "{stderr}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.lines().any(|line| line.starts_with("SPOT: W1AW ")),
        "{stdout}"
    );
}

#[test]
fn text_helper_exceptions_survive_off_while_json_helpers_are_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = server_config(
        dir.path(),
        "[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7000000.0\n",
    );
    let text = daemon_with(dir.path(), &cfg, &["-qqq"], &[]);
    let stderr = stderr_of(&text);
    assert!(
        stderr.contains("note: --source selects the source"),
        "{stderr}"
    );
    assert!(stderr.contains(READY), "{stderr}");
    assert!(
        !stderr.contains(" INFO ") && !stderr.contains(" WARN "),
        "{stderr}"
    );
    let structured = daemon_with(dir.path(), &cfg, &["-qqq", "--log-format", "json"], &[]);
    assert!(stderr_of(&structured).is_empty());
    assert_eq!(text.stdout, structured.stdout);
}
