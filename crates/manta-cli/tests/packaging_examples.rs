//! MAN-268: the unattended-service kit stays in step with the code. The root
//! `manta.example.toml` is `manta config init`'s scaffold with only the
//! documented edits; an unedited copy cannot start a daemon, and an edited one
//! starts and stops cleanly on SIGTERM. The systemd unit, Compose file and
//! LaunchDaemon run that config and give the SIGTERM drain time to finish.
//! See docs/DECISIONS/2026-10-10-man268-unattended-packaging.md.

use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EXAMPLE: &str = "manta.example.toml";
const SYSTEMD_UNIT: &str = "packaging/systemd/manta.service";
const COMPOSE: &str = "docker-compose.yml";
const LAUNCHD_PLIST: &str = "packaging/launchd/com.hagaletechnologies.manta.plist";

/// `config_cmd`'s station diagnostic, shared by `config check` and `run`.
const STATION_REFUSAL: &str =
    "server.station_callsign is still the example \"N0CALL\" -- set your own callsign";

/// The example's one active setting line, which an operator edits.
const ACTIVE_STATION: &str = "\nstation_callsign = \"N0CALL\"\n";

/// `run`'s guard for a `[server]` config whose audio/file source has no dial
/// frequency: the next check after the station guard.
const DIAL_REQUIRED: &str = "--dial-freq-hz is required with --config";

/// Seconds of margin every service manager's stop timeout must leave over
/// manta's own drain and runtime shutdown (the MAN-96 field-unit precedent).
const STOP_MARGIN_SECS: u64 = 5;

/// The documented edits that turn `manta config init --out -` into
/// manta.example.toml: a header that names the file, the station identity
/// made active, and the prose describing both. Each `from` must occur
/// exactly once, so scaffold drift fails here instead of skipping an edit.
const EXAMPLE_EDITS: &[(&str, &str)] = &[
    (
        "# manta configuration file, written by `manta config init`.\n",
        "# manta.example.toml: copy to manta.toml and set your station callsign.\n\
         # Configure your receiver before running. Audio/file sources also need the\n\
         # actual radio dial frequency; see packaging/README.md.\n\
         # Derived from `manta config init --out -`; required station identity is active.\n",
    ),
    (
        "# Every setting manta reads is listed below, commented out and set to its\n\
         # built-in default. To change one, delete the `#` at the start of its line\n",
        "# Every setting manta reads is listed below. The required station identity\n\
         # is active; every other setting is commented out at its built-in default.\n\
         # To change a commented setting, delete the `#` at the start of its line\n",
    ),
    (
        "# file has a [server] table: delete the `#` in front of [server] and of\n\
         # station_callsign, and set your callsign.\n",
        "# file has a [server] table. It is enabled below; replace N0CALL with your call.\n",
    ),
    ("\n#[server]\n", "\n[server]\n"),
    (
        "\n#station_callsign = \"N0CALL\"\n",
        "\n#station_callsign = \"N0CALL\"\n\
         # Edit the active line below; do not uncomment the reference line above.\n\
         station_callsign = \"N0CALL\"\n",
    ),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// A repository file, with CRLF (a Windows checkout) folded to LF.
fn doc(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
        .replace("\r\n", "\n")
}

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    // `run` and `config check` read `MANTA_CONFIG` and `MANTA_<TABLE>_<KEY>`
    // and reject unknown `MANTA_*` names, so the test runner's own
    // environment must never leak into the child. Tests that exercise the
    // env tier set theirs explicitly.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `config init --out -`: the scaffold the example is derived from.
fn scaffold() -> String {
    let o = manta()
        .args(["config", "init", "--out", "-"])
        .output()
        .unwrap();
    assert!(o.status.success(), "config init failed: {}", stderr(&o));
    stdout(&o).replace("\r\n", "\n")
}

/// The shipped example written to `dir/manta.toml`, with its active station
/// line set to `call` (`None` leaves it unedited) and `input` appended to
/// its `[input]` table.
fn example_copy(dir: &Path, call: Option<&str>, input: &str) -> PathBuf {
    let mut text = doc(EXAMPLE);
    assert_eq!(
        text.matches(ACTIVE_STATION).count(),
        1,
        "{EXAMPLE} must have exactly one active station_callsign line"
    );
    if let Some(call) = call {
        text = text.replacen(
            ACTIVE_STATION,
            &format!("\nstation_callsign = \"{call}\"\n"),
            1,
        );
    }
    assert_eq!(text.matches("\n[input]\n").count(), 1);
    text = text.replacen("\n[input]\n", &format!("\n[input]\n{input}"), 1);
    let path = dir.join("manta.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// `manta <args>` run in `dir`, so `--config manta.toml` names `dir`'s copy.
fn manta_in(dir: &Path, args: &[&str]) -> Output {
    manta().current_dir(dir).args(args).output().unwrap()
}

/// Exit 1 before any listener: nothing on stdout, no `listening:` banner,
/// and the station key named. Returns stderr.
fn assert_station_refused(o: &Output, what: &str) -> String {
    let err = stderr(o);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{what}: expected exit 1\nstdout: {}\nstderr: {err}",
        stdout(o)
    );
    assert!(err.contains(STATION_REFUSAL), "{what}: {err}");
    assert!(
        !err.contains("listening:"),
        "{what} started listening: {err}"
    );
    assert!(stdout(o).is_empty(), "{what}: stdout {:?}", stdout(o));
    err
}

/// Panics at the first line where `actual` and `expected` differ.
fn assert_same_lines(actual: &str, expected: &str, what: &str) {
    let (a, e): (Vec<&str>, Vec<&str>) = (actual.lines().collect(), expected.lines().collect());
    if let Some(n) = (0..a.len().max(e.len())).find(|&n| a.get(n) != e.get(n)) {
        panic!(
            "{what} differs at line {}:\n  actual:   {:?}\n  expected: {:?}",
            n + 1,
            a.get(n),
            e.get(n)
        );
    }
    assert_eq!(actual, expected, "{what}: trailing newline differs");
}

/// `const NAME: ... from_secs(N)`-style seconds: the `from_secs(N)` on the
/// first line of crates/manta-cli/src/main.rs that contains `needle`.
fn main_rs_secs(needle: &str) -> u64 {
    let src = doc("crates/manta-cli/src/main.rs");
    let line = src
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("main.rs has no line containing {needle:?}"));
    let start = line
        .find("from_secs(")
        .unwrap_or_else(|| panic!("{needle:?} is not `from_secs(N)`: {line}"))
        + "from_secs(".len();
    let len = line[start..]
        .find(')')
        .expect("from_secs( is closed with `)`");
    line[start..start + len]
        .trim()
        .replace('_', "")
        .parse()
        .unwrap_or_else(|e| panic!("seconds in {line:?}: {e}"))
}

/// The least stop timeout a service manager may give manta: the client
/// drain (`SHUTDOWN_DRAIN_DEADLINE`), then the runtime's own shutdown
/// timeout, then a margin.
fn stop_budget_floor() -> u64 {
    let drain = main_rs_secs("const SHUTDOWN_DRAIN_DEADLINE:");
    let runtime = main_rs_secs("rt.shutdown_timeout(");
    drain + runtime + STOP_MARGIN_SECS
}

/// The loader's default `(telnet, json, metrics)` ports, read from `config
/// check`'s summary of a minimal `[server]`, so they cannot drift from code.
fn default_ports() -> (u16, u16, u16) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manta.toml");
    std::fs::write(&path, "[server]\nstation_callsign = \"W3XYZ\"\n").unwrap();
    let o = manta()
        .args(["config", "check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    let port = |key: &str| -> u16 {
        Regex::new(&format!(r" {key}=(\d+)"))
            .unwrap()
            .captures(&out)
            .unwrap_or_else(|| panic!("no {key} in {out}"))[1]
            .parse()
            .unwrap()
    };
    (port("telnet_port"), port("json_port"), port("metrics_port"))
}

// ---- manta.example.toml

#[test]
fn example_is_the_scaffold_with_only_the_documented_edits() {
    let mut expected = scaffold();
    for (from, to) in EXAMPLE_EDITS {
        assert_eq!(
            expected.matches(from).count(),
            1,
            "`config init` output must contain {from:?} exactly once; update EXAMPLE_EDITS and \
             {EXAMPLE} together"
        );
        expected = expected.replacen(from, to, 1);
    }
    assert_same_lines(&doc(EXAMPLE), &expected, EXAMPLE);
}

#[test]
fn example_keeps_every_commented_setting_of_the_scaffold() {
    let setting = Regex::new(r"^#[a-z_]+ = ").unwrap();
    let example = doc(EXAMPLE);
    let lines: BTreeSet<&str> = example.lines().collect();
    let scaffold = scaffold();
    let settings: Vec<&str> = scaffold.lines().filter(|l| setting.is_match(l)).collect();
    assert!(settings.len() >= 60, "only {} settings", settings.len());
    let missing: Vec<&&str> = settings.iter().filter(|l| !lines.contains(*l)).collect();
    assert!(
        missing.is_empty(),
        "{EXAMPLE} lost scaffold settings: {missing:#?}"
    );
}

/// The example ships no real identity, no active receiver and no assumed RF
/// frequency: its only active value is the placeholder station.
#[test]
fn example_activates_only_the_placeholder_station() {
    let table: toml::Table = toml::from_str(&doc(EXAMPLE)).expect("the example parses as TOML");
    let tables: BTreeSet<&str> = table.keys().map(String::as_str).collect();
    assert_eq!(
        tables,
        BTreeSet::from(["decode", "detector", "input", "server", "spot"])
    );
    for (name, value) in &table {
        let t = value
            .as_table()
            .unwrap_or_else(|| panic!("{name} is not a table"));
        if name == "server" {
            let keys: Vec<&String> = t.keys().collect();
            assert_eq!(keys, ["station_callsign"], "[server] sets only the station");
            assert_eq!(t["station_callsign"].as_str(), Some("N0CALL"));
        } else {
            assert!(
                t.is_empty(),
                "[{name}] sets {t:?}; only the station is active"
            );
        }
    }
}

#[test]
fn config_check_refuses_the_unedited_example_and_accepts_a_real_call() {
    let dir = tempfile::tempdir().unwrap();
    example_copy(dir.path(), None, "");
    let o = manta_in(dir.path(), &["config", "check", "--config", "manta.toml"]);
    assert_station_refused(&o, "config check");

    example_copy(dir.path(), Some("W3XYZ"), "");
    let o = manta_in(dir.path(), &["config", "check", "--config", "manta.toml"]);
    assert!(
        o.status.success(),
        "an example with only its callsign edited must pass check\nstdout: {}\nstderr: {}",
        stdout(&o),
        stderr(&o)
    );
    assert!(
        stdout(&o).contains("\nserver: station_callsign=W3XYZ "),
        "{}",
        stdout(&o)
    );
}

// ---- `run` refuses the unedited example before any source or listener

#[test]
fn run_refuses_the_unedited_example_before_the_dial_frequency_check() {
    let dir = tempfile::tempdir().unwrap();
    example_copy(dir.path(), None, "");
    let o = manta_in(dir.path(), &["run", "--config", "manta.toml"]);
    let err = assert_station_refused(&o, "run");
    // The complete output contract, with no MANTA_* override applied.
    assert_eq!(err, format!("Error: manta.toml: {STATION_REFUSAL}\n"));
    assert!(!err.contains(DIAL_REQUIRED), "{err}");
}

#[test]
fn run_refuses_the_example_station_in_any_case() {
    let dir = tempfile::tempdir().unwrap();
    example_copy(dir.path(), Some("n0call"), "");
    let o = manta_in(dir.path(), &["run", "--config", "manta.toml"]);
    assert_station_refused(&o, "run with n0call");
}

#[test]
fn listen_alias_refuses_the_unedited_example() {
    let dir = tempfile::tempdir().unwrap();
    example_copy(dir.path(), None, "");
    let o = manta_in(dir.path(), &["listen", "--config", "manta.toml"]);
    let err = assert_station_refused(&o, "listen");
    // `listen` first warns that it is deprecated; the refusal ends stderr.
    assert_eq!(
        err.lines().last(),
        Some(format!("Error: manta.toml: {STATION_REFUSAL}").as_str()),
        "{err}"
    );
}

/// A missing WAV is a sentinel for source I/O: with a real call the same
/// config fails opening it, so the station refusal really comes first.
#[test]
fn station_refusal_precedes_opening_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let file_input = "type = \"file\"\npath = \"missing.wav\"\ncenter_freq_hz = 14060000.0\n";
    let source_flag = ["--source", "missing.wav", "--dial-freq-hz", "14060000"];
    for (what, input, flags) in [
        ("[input] type = \"file\"", file_input, &[][..]),
        ("--source", "", &source_flag[..]),
    ] {
        let args: Vec<&str> = ["run", "--config", "manta.toml"]
            .into_iter()
            .chain(flags.iter().copied())
            .collect();

        example_copy(dir.path(), Some("W3XYZ"), input);
        let o = manta_in(dir.path(), &args);
        let err = stderr(&o);
        assert_eq!(o.status.code(), Some(1), "{what}, real call: {err}");
        assert!(
            err.contains("(os error 2)") && !err.contains("station_callsign"),
            "{what}: with a real call, run must reach the missing source: {err}"
        );

        example_copy(dir.path(), None, input);
        let o = manta_in(dir.path(), &args);
        let err = assert_station_refused(&o, what);
        assert!(!err.contains("(os error 2)"), "{what}: {err}");
    }
}

// ---- an edited example starts, and SIGTERM stops it cleanly

#[cfg(unix)]
mod startup {
    use super::*;
    use std::io::BufRead as _;
    use std::process::{Child, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Replay seconds in the fixture. Only has to outlast the test window;
    /// the unsignalled control turns "too short" into a failure, not a pass.
    /// The dev profile decodes this noise about 45x faster than real time,
    /// so 600 s lasts roughly 13 s against a test window well under 1 s.
    const FIXTURE_SECONDS: u32 = 600;
    const BANNER_BUDGET: Duration = Duration::from_secs(60);
    /// Signal -> exit with no client attached takes milliseconds; this only
    /// has to stay far below the fixture's natural end.
    const EXIT_BUDGET: Duration = Duration::from_secs(15);

    /// Low-level noise as a mono 16-bit 48 kHz WAV, the format a rig's audio
    /// line-out recording has.
    fn write_fixture(path: &Path) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let samples = FIXTURE_SECONDS * spec.sample_rate;
        let mut wav = hound::WavWriter::create(path, spec).unwrap();
        let mut w = wav.get_i16_writer(samples);
        let mut state: u32 = 0x1234_5678;
        for _ in 0..samples {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            w.write_sample(((state >> 16) as i16) >> 6);
        }
        w.flush().unwrap();
        wav.finalize().unwrap();
    }

    /// A spawned daemon whose stderr lines arrive on `lines` and whose
    /// stdout is drained, so neither pipe can fill and block it. Dropping it
    /// kills and reaps the process, so a failed assertion leaks nothing.
    struct Daemon {
        child: Child,
        lines: mpsc::Receiver<String>,
        seen: Vec<String>,
    }

    impl Daemon {
        fn spawn(dir: &Path, wav: &Path) -> Daemon {
            let mut child = manta()
                .current_dir(dir)
                .args(["run", "--config", "manta.toml", "--source"])
                .arg(wav)
                .args(["--dial-freq-hz", "14060000"])
                // Loopback and kernel-assigned ports: this may run beside
                // anything else in the workspace.
                .env("MANTA_SERVER_BIND_ADDR", "127.0.0.1")
                .env("MANTA_SERVER_METRICS_BIND_ADDR", "127.0.0.1")
                .env("MANTA_SERVER_TELNET_PORT", "0")
                .env("MANTA_SERVER_JSON_PORT", "0")
                .env("MANTA_SERVER_METRICS_PORT", "0")
                .env("MANTA_SERVER_STATUS_INTERVAL_SECS", "0")
                .env("RUST_LOG", "info")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut out = child.stdout.take().unwrap();
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut out, &mut std::io::sink());
            });
            let err = std::io::BufReader::new(child.stderr.take().unwrap());
            let (tx, lines) = mpsc::channel();
            std::thread::spawn(move || {
                for line in err.lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        // Receiver gone: keep draining so the child never
                        // hits EPIPE on a later `eprintln!`.
                        continue;
                    }
                }
            });
            Daemon {
                child,
                lines,
                seen: Vec::new(),
            }
        }

        /// The `listening:` startup banner, waiting at most `BANNER_BUDGET`.
        fn banner(&mut self) -> String {
            let deadline = Instant::now() + BANNER_BUDGET;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.lines.recv_timeout(left) {
                    Ok(line) => {
                        self.seen.push(line.clone());
                        // `listening:` (colon) is the banner; the later
                        // ready marker is `manta: listening;`.
                        if line.contains("listening:") {
                            return line;
                        }
                    }
                    Err(e) => panic!(
                        "no `listening:` banner ({e:?}; exit {:?}); stderr so far:\n{}",
                        self.child.try_wait(),
                        self.seen.join("\n")
                    ),
                }
            }
        }

        fn running(&mut self) -> bool {
            self.child.try_wait().unwrap().is_none()
        }

        fn wait_bounded(&mut self, budget: Duration) -> Option<std::process::ExitStatus> {
            let start = Instant::now();
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return Some(status);
                }
                if start.elapsed() > budget {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            if let Ok(None) = self.child.try_wait() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }

    #[test]
    fn edited_example_starts_listening_and_exits_zero_on_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        example_copy(dir.path(), Some("W3XYZ"), "");
        let wav = dir.path().join("audio.wav");
        write_fixture(&wav);

        // `control` runs the same command unsignalled. Without it a fixture
        // that reached its end inside the test window would exit 0 on its
        // own and pass even with SIGTERM unhandled.
        let mut control = Daemon::spawn(dir.path(), &wav);
        let mut child = Daemon::spawn(dir.path(), &wav);
        control.banner();
        let banner = child.banner();
        for needle in [
            "station=W3XYZ",
            "dial_freq_hz=14060000",
            "telnet=127.0.0.1:",
            "json=127.0.0.1:",
            "metrics=127.0.0.1:",
        ] {
            assert!(banner.contains(needle), "banner lacks {needle:?}: {banner}");
        }

        assert!(child.running(), "manta exited before it was signalled");
        assert_eq!(
            unsafe { libc::kill(child.child.id() as i32, libc::SIGTERM) },
            0,
            "kill(SIGTERM) failed: {}",
            std::io::Error::last_os_error()
        );
        let status = child
            .wait_bounded(EXIT_BUDGET)
            .unwrap_or_else(|| panic!("SIGTERM did not stop manta within {EXIT_BUDGET:?}"));
        assert!(
            control.running(),
            "an unsignalled run of the same fixture also finished, so this test cannot \
             attribute the exit to SIGTERM -- raise FIXTURE_SECONDS"
        );
        assert_eq!(
            status.code(),
            Some(0),
            "SIGTERM must run the shutdown drain and exit 0, got {status:?}"
        );
    }
}

// ---- service managers

/// A systemd unit file as `(section, key, value)` triples, in file order.
/// Comments (`#`, `;`) and blank lines are dropped; backslash continuations
/// are joined. (Copied from tests/field_node_runbook.rs.)
fn unit_entries(text: &str) -> Vec<(String, String, String)> {
    let mut joined: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(head) = trimmed.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(trimmed);
        joined.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        joined.push(pending);
    }

    let mut section = String::new();
    let mut out = Vec::new();
    for line in joined {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_string();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.push((
                section.clone(),
                key.trim().to_string(),
                value.trim().to_string(),
            ));
        }
    }
    out
}

/// The last value of `key` in `[section]` (systemd's last-assignment-wins rule).
fn unit_value(entries: &[(String, String, String)], section: &str, key: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|(s, k, _)| s == section && k == key)
        .map(|(_, _, v)| v.clone())
}

/// A systemd time span in whole seconds: a bare number, or `Ns` / `Nmin` / `Nm`.
fn systemd_seconds(value: &str) -> u64 {
    let v = value.trim();
    let parse = |n: &str| -> u64 {
        n.trim()
            .parse()
            .unwrap_or_else(|e| panic!("time span {value:?}: {e}"))
    };
    if let Some(n) = v.strip_suffix("min") {
        parse(n) * 60
    } else if let Some(n) = v.strip_suffix('m') {
        parse(n) * 60
    } else if let Some(n) = v.strip_suffix('s') {
        parse(n)
    } else {
        parse(v)
    }
}

#[test]
fn systemd_unit_runs_the_credential_config_and_outlasts_the_drain() {
    let entries = unit_entries(&doc(SYSTEMD_UNIT));
    let service = |key: &str| unit_value(&entries, "Service", key);

    // No quoting in this ExecStart, so whitespace splits it exactly.
    let exec = service("ExecStart").expect("[Service] ExecStart");
    let words: Vec<&str> = exec.split_whitespace().collect();
    assert_eq!(
        words,
        [
            "/usr/local/bin/manta",
            "run",
            "--config",
            "${CREDENTIALS_DIRECTORY}/manta.toml"
        ]
    );
    // The credential ExecStart reads is the one LoadCredential provides.
    let credential = service("LoadCredential").expect("[Service] LoadCredential");
    let (id, source) = credential
        .split_once(':')
        .unwrap_or_else(|| panic!("LoadCredential={credential} is not ID:PATH"));
    assert_eq!(
        words[3].strip_prefix("${CREDENTIALS_DIRECTORY}/"),
        Some(id),
        "ExecStart's --config must be the loaded credential"
    );
    assert_eq!(source, "/etc/manta/manta.toml");
    // MAN-78: `systemctl reload manta` is a SIGHUP, which a `[server]`
    // daemon answers by re-reading its [spot] lists and dry_run.
    assert_eq!(
        service("ExecReload").as_deref(),
        Some("/bin/kill -HUP $MAINPID")
    );

    assert_eq!(service("Type").as_deref(), Some("simple"));
    assert_eq!(service("DynamicUser").as_deref(), Some("yes"));
    for key in ["User", "Group"] {
        assert_eq!(service(key), None, "DynamicUser allocates the identity");
    }
    assert_eq!(service("Restart").as_deref(), Some("always"));
    assert_eq!(service("RestartSec").as_deref(), Some("10"));
    assert_eq!(
        unit_value(&entries, "Unit", "StartLimitIntervalSec").as_deref(),
        Some("0"),
        "systemd must never give up restarting manta"
    );
    for key in ["StandardOutput", "StandardError"] {
        assert_eq!(service(key).as_deref(), Some("journal"), "{key}");
    }
    // manta drains on systemd's default SIGTERM (MAN-85); nothing may
    // retarget or shorten that.
    for key in [
        "KillSignal",
        "KillMode",
        "SendSIGKILL",
        "FinalKillSignal",
        "RestartKillSignal",
        "TimeoutSec",
    ] {
        assert_eq!(service(key), None, "{SYSTEMD_UNIT} must not set {key}");
    }
    let stop = systemd_seconds(&service("TimeoutStopSec").expect("[Service] TimeoutStopSec"));
    let floor = stop_budget_floor();
    assert!(
        stop >= floor,
        "TimeoutStopSec={stop} must be at least {floor} s (drain + runtime shutdown + \
         {STOP_MARGIN_SECS} s), or systemd SIGKILLs a draining daemon"
    );
    assert_eq!(
        unit_value(&entries, "Install", "WantedBy").as_deref(),
        Some("multi-user.target")
    );
}

/// A Compose duration (`60s`, `1m30s`, `1h`) in whole seconds.
fn compose_seconds(value: &str) -> u64 {
    let part = Regex::new(r"(\d+)(us|ms|h|m|s)").unwrap();
    assert!(
        Regex::new(r"^(?:\d+(?:us|ms|h|m|s))+$")
            .unwrap()
            .is_match(value),
        "{value:?} is not a Compose duration"
    );
    let micros: u64 = part
        .captures_iter(value)
        .map(|c| {
            let n: u64 = c[1].parse().unwrap();
            n * match &c[2] {
                "us" => 1,
                "ms" => 1_000,
                "s" => 1_000_000,
                "m" => 60_000_000,
                "h" => 3_600_000_000,
                other => unreachable!("{other}"),
            }
        })
        .sum();
    micros / 1_000_000
}

#[test]
fn compose_runs_the_mounted_config_and_outlasts_the_drain() {
    let compose: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&doc(COMPOSE)).expect("docker-compose.yml parses as YAML");
    let services = compose["services"]
        .as_mapping()
        .expect("a services mapping");
    assert_eq!(services.len(), 1, "one service");
    let manta = &compose["services"]["manta"];
    let str_of = |v: &serde_yaml_ng::Value| v.as_str().map(str::to_string);

    assert_eq!(
        str_of(&manta["image"]).as_deref(),
        Some("ghcr.io/hagaletechnologies/manta:latest")
    );
    assert_eq!(str_of(&manta["restart"]).as_deref(), Some("unless-stopped"));
    let command: Vec<String> = manta["command"]
        .as_sequence()
        .expect("command is an exec-form list")
        .iter()
        .map(|v| str_of(v).expect("command words are strings"))
        .collect();
    assert_eq!(command, ["run", "--config", "/etc/manta/manta.toml"]);

    // `command` is only manta's arguments if the image's entrypoint is manta.
    let dockerfile = doc("Dockerfile");
    let entrypoint = dockerfile
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("ENTRYPOINT"))
        .expect("the Dockerfile sets an ENTRYPOINT");
    let entrypoint: Vec<String> =
        serde_json::from_str(entrypoint.trim()).expect("an exec-form ENTRYPOINT");
    assert_eq!(entrypoint, ["manta"]);

    let grace = compose_seconds(&str_of(&manta["stop_grace_period"]).expect("stop_grace_period"));
    let floor = stop_budget_floor();
    assert!(
        grace >= floor,
        "stop_grace_period {grace} s must be at least {floor} s (drain + runtime shutdown + \
         {STOP_MARGIN_SECS} s), or Docker SIGKILLs a draining daemon"
    );
    assert!(
        manta.get("stop_signal").is_none(),
        "manta drains on Docker's default SIGTERM (MAN-85)"
    );

    let logging = &manta["logging"];
    assert_eq!(str_of(&logging["driver"]).as_deref(), Some("json-file"));
    assert_eq!(
        str_of(&logging["options"]["max-size"]).as_deref(),
        Some("10m")
    );
    assert_eq!(
        str_of(&logging["options"]["max-file"]).as_deref(),
        Some("3")
    );

    let volumes = manta["volumes"].as_sequence().expect("a volumes list");
    assert_eq!(volumes.len(), 1, "one mount: the config file");
    let config = &volumes[0];
    assert_eq!(str_of(&config["type"]).as_deref(), Some("bind"));
    assert_eq!(str_of(&config["source"]).as_deref(), Some("./manta.toml"));
    assert_eq!(
        str_of(&config["target"]).as_deref(),
        Some(command[2].as_str())
    );
    assert_eq!(config["read_only"].as_bool(), Some(true));
    assert_eq!(
        config["bind"]["create_host_path"].as_bool(),
        Some(false),
        "a missing ./manta.toml must fail, not become an empty directory"
    );
    assert_eq!(str_of(&config["bind"]["selinux"]).as_deref(), Some("Z"));

    // Metrics listens on the container's 0.0.0.0 so a published port can
    // reach it, and is published on the host's loopback only (MAN-132).
    let env = &manta["environment"];
    assert_eq!(
        str_of(&env["MANTA_SERVER_METRICS_BIND_ADDR"]).as_deref(),
        Some("0.0.0.0")
    );
    assert_eq!(str_of(&env["RUST_LOG"]).as_deref(), Some("info"));
    let (telnet, json, metrics) = default_ports();
    let ports: BTreeSet<String> = manta["ports"]
        .as_sequence()
        .expect("a ports list")
        .iter()
        .map(|v| str_of(v).expect("short-syntax port strings"))
        .collect();
    assert_eq!(
        ports,
        BTreeSet::from([
            format!("{telnet}:{telnet}"),
            format!("{json}:{json}"),
            format!("127.0.0.1:{metrics}:{metrics}"),
        ]),
        "ports must match the config defaults, metrics on host loopback only"
    );

    for key in ["privileged", "network_mode", "devices", "cap_add", "pid"] {
        assert!(manta.get(key).is_none(), "{COMPOSE} must not set {key}");
    }
}

#[test]
fn launchd_exit_timeout_outlasts_the_drain() {
    let plist = doc(LAUNCHD_PLIST);
    let exit_timeout =
        Regex::new(r"<key>\s*ExitTimeOut\s*</key>\s*<integer>\s*(\d+)\s*</integer>").unwrap();
    let values: Vec<u64> = exit_timeout
        .captures_iter(&plist)
        .map(|c| c[1].parse().unwrap())
        .collect();
    assert_eq!(
        values.len(),
        1,
        "{LAUNCHD_PLIST} sets ExitTimeOut once: {values:?}"
    );
    let floor = stop_budget_floor();
    assert!(
        values[0] >= floor,
        "ExitTimeOut {} must be at least {floor} s (drain + runtime shutdown + \
         {STOP_MARGIN_SECS} s), or launchd SIGKILLs a draining daemon",
        values[0]
    );
}
