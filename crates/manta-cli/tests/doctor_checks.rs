//! MAN-126 acceptance against the real `manta doctor` binary: a healthy
//! setup passes every check (scenario 1), and each real problem is named
//! with its fix (scenario 2). Every network check runs against a local fake
//! (TCP listener, SNTP responder, held or closed port), and every run
//! passes `--ntp-server 127.0.0.1:<fake>`, so no test reaches the internet.
//!
//! Helpers are copied from `metrics_bind.rs`; the repo's test files are
//! self-contained.

use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// `seconds` of 48 kHz mono silence: enough for doctor's 3 s minimum.
fn silent_48k_wav(dir: &Path, seconds: u32) -> PathBuf {
    let path = dir.join("silence.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for _ in 0..(48_000 * seconds) {
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
    path
}

/// A port nothing listens on right now (bound, read, released).
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A UDP port nothing answers on, for an NTP server that refuses.
fn closed_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const NTP_UNIX_OFFSET: f64 = 2_208_988_800.0;

fn ntp_bytes(unix: f64) -> [u8; 8] {
    let ntp = unix + NTP_UNIX_OFFSET;
    let secs = ntp.trunc() as u64 as u32;
    let frac = (ntp.fract() * 4_294_967_296.0) as u32;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&secs.to_be_bytes());
    out[4..].copy_from_slice(&frac.to_be_bytes());
    out
}

/// A loopback SNTP server whose clock runs `offset_s` ahead of this one,
/// echoing each request's transmit time as the originate time.
fn fake_sntp(offset_s: f64) -> (u16, std::thread::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n < 48 {
                continue;
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64()
                + offset_s;
            let mut reply = [0u8; 48];
            reply[0] = 0x24; // LI 0, VN 4, mode 4 (server)
            reply[1] = 1; // stratum 1
            reply[24..32].copy_from_slice(&buf[40..48]);
            reply[32..40].copy_from_slice(&ntp_bytes(now));
            reply[40..48].copy_from_slice(&ntp_bytes(now));
            let _ = sock.send_to(&reply, from);
        }
    });
    (port, handle)
}

/// The host kernel's own NTP-sync state, read the way doctor reads it, so
/// no expectation depends on the CI runner's time daemon. `None` off Linux.
fn host_synced() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: all-zero is a valid `timex`; `modes = 0` only reads.
        let mut tx: libc::timex = unsafe { std::mem::zeroed() };
        let state = unsafe { libc::adjtimex(&mut tx) };
        if state < 0 {
            return None;
        }
        Some(state != libc::TIME_ERROR && tx.status & libc::STA_UNSYNC == 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Everything a healthy setup has: three free loopback ports, one enabled
/// uplink at a live local listener, and a WAV receiver with a dial
/// frequency.
struct Setup {
    dir: tempfile::TempDir,
    ports: [u16; 3],
    uplink: TcpListener,
    wav: PathBuf,
    station: &'static str,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wav = silent_48k_wav(dir.path(), 4);
        Self {
            dir,
            ports: [free_port(), free_port(), free_port()],
            uplink: TcpListener::bind("127.0.0.1:0").unwrap(),
            wav,
            station: "W1AW",
        }
    }

    fn uplink_port(&self) -> u16 {
        self.uplink.local_addr().unwrap().port()
    }

    fn config(&self, uplink_port: u16) -> PathBuf {
        let [telnet, json, metrics] = self.ports;
        let body = format!(
            "[server]\nstation_callsign = \"{}\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet}\njson_port = {json}\n\
             metrics_port = {metrics}\n\n[[rbn_uplink]]\nenabled = true\n\
             target_host = \"127.0.0.1\"\ntarget_port = {uplink_port}\n\n[input]\n\
             type = \"file\"\npath = \"{}\"\ncenter_freq_hz = 14030000.0\n",
            self.station,
            self.wav.display().to_string().replace('\\', "\\\\")
        );
        let path = self.dir.path().join("manta.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn doctor(&self, cfg: &Path, ntp_port: u16, extra: &[&str]) -> Output {
        manta()
            .args(["doctor", "--duration", "3", "--ntp-server"])
            .arg(format!("127.0.0.1:{ntp_port}"))
            .arg("--config")
            .arg(cfg)
            .args(extra)
            .output()
            .unwrap()
    }
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The clock line a fake server at offset 0 earns on this host.
fn assert_clock_line_for_a_good_server(stdout: &str) {
    if host_synced() == Some(false) {
        assert!(
            stdout.contains("WARN  clock: within")
                && stdout.contains("nothing is keeping it in sync"),
            "{stdout}"
        );
    } else {
        assert!(stdout.contains("PASS  clock: within"), "{stdout}");
    }
}

/// The line right after the first line starting with `prefix`.
fn line_after<'a>(stdout: &'a str, prefix: &str) -> &'a str {
    let mut lines = stdout.lines();
    lines
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {stdout}"));
    lines.next().unwrap_or_default()
}

#[test]
fn healthy_setup_passes_every_check() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    let [telnet, json, metrics] = setup.ports;
    for needle in [
        "PASS  config:".to_string(),
        "PASS  audio:".to_string(),
        format!("PASS  telnet port: 127.0.0.1:{telnet} is free"),
        format!("PASS  json port: 127.0.0.1:{json} is free"),
        format!("PASS  metrics port: 127.0.0.1:{metrics} is free"),
        format!(
            "PASS  rbn uplink 127.0.0.1:{}: accepted a connection",
            setup.uplink_port()
        ),
        "PASS  receiver: opened the WAV file".to_string(),
        "verdict: ".to_string(),
    ] {
        assert!(stdout.contains(&needle), "no {needle:?} in {stdout}");
    }
    assert_clock_line_for_a_good_server(&stdout);
    if host_synced() == Some(false) {
        assert!(stdout.contains("doctor: 1 warning: clock"), "{stdout}");
    } else {
        assert!(stdout.contains("doctor: all checks passed"), "{stdout}");
    }
    assert!(!stderr.contains("ignoring [server]"), "{stderr}");
}

#[test]
fn a_held_port_is_named_with_its_fix() {
    let mut setup = Setup::new();
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    setup.ports[0] = held.local_addr().unwrap().port();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let line = format!(
        "FAIL  telnet port: 127.0.0.1:{} is already in use",
        setup.ports[0]
    );
    assert!(stdout.contains(&line), "{stdout}");
    let fix = line_after(&stdout, &line);
    assert!(
        fix.starts_with("      fix: ") && fix.contains("telnet_port"),
        "{fix}"
    );
    assert!(stdout.contains("failed: telnet port"), "{stdout}");
    assert!(stdout.contains("verdict: "), "{stdout}");
    assert!(!stderr.contains("Error:"), "{stderr}");
    drop(held);
}

#[test]
fn an_unreachable_uplink_is_named_with_its_fix() {
    let setup = Setup::new();
    let closed = free_port();
    let cfg = setup.config(closed);
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let prefix = format!("FAIL  rbn uplink 127.0.0.1:{closed}: refused the connection");
    assert!(stdout.contains(&prefix), "{stdout}");
    assert!(
        line_after(&stdout, &prefix).contains("target_port"),
        "{stdout}"
    );
}

#[test]
fn a_clock_two_minutes_off_fails() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(120.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let line = stdout
        .lines()
        .find(|l| l.starts_with("FAIL  clock: this clock is "))
        .unwrap_or_else(|| panic!("no clock FAIL in {stdout}"));
    assert!(
        line.contains(&format!(
            " s behind 127.0.0.1:{ntp}, so every spot's time is wrong"
        )),
        "{line}"
    );
    let secs: f64 = line["FAIL  clock: this clock is ".len()..]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((119.0..121.0).contains(&secs), "{line}");
}

#[test]
fn an_unreachable_ntp_server_is_a_warning_not_a_failure() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let out = setup.doctor(&cfg, closed_port(), &[]);
    let (stdout, stderr) = text(&out);
    if host_synced() == Some(true) {
        assert!(
            stdout.contains("PASS  clock: ")
                && stdout.contains("NTP is keeping this clock in sync"),
            "{stdout}"
        );
    } else {
        assert!(
            stdout.contains("WARN  clock: could not measure the clock: 127.0.0.1:"),
            "{stdout}"
        );
    }
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn a_receiver_that_cannot_open_is_a_named_check() {
    let (ntp, _h) = fake_sntp(0.0);
    let out = manta()
        .args(["doctor", "--duration", "3", "--source", "/nonexistent.wav"])
        .arg("--ntp-server")
        .arg(format!("127.0.0.1:{ntp}"))
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let prefix = "FAIL  receiver: could not open the WAV file /nonexistent.wav:";
    assert!(stdout.contains(prefix), "{stdout}");
    assert!(line_after(&stdout, prefix).contains("--source"), "{stdout}");
    assert!(!stdout.contains("verdict:"), "{stdout}");
    assert!(!stderr.contains("Error:"), "{stderr}");
}

#[test]
fn the_example_callsign_fails_the_config_check() {
    let mut setup = Setup::new();
    setup.station = "N0CALL";
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("FAIL  config: server.station_callsign is still the example \"N0CALL\""),
        "{stdout}"
    );
}

/// The keys `manta doctor --json` printed before MAN-126.
const REPORT_KEYS: &[&str] = &[
    "center_freq_hz",
    "chars_decoded",
    "distinct_chars",
    "duration",
    "sample_rate_hz",
    "snr_db_max",
    "snr_db_median",
    "snr_db_min",
    "spots_confirmed",
    "track_meta_count",
    "tracks_closed",
    "tracks_promoted",
    "verdict",
];

fn json_object(stdout: &str) -> serde_json::Map<String, serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("doctor --json not one JSON object ({e}): {stdout}"));
    value.as_object().unwrap().clone()
}

#[test]
fn json_adds_checks_and_keeps_every_existing_key() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &["--json"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    let map = json_object(&stdout);
    for key in REPORT_KEYS {
        assert!(map.contains_key(*key), "no {key} in {stdout}");
    }
    assert_eq!(map.len(), REPORT_KEYS.len() + 2, "{stdout}");
    let checks = map["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    for check in checks {
        let mut keys: Vec<&str> = check
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["detail", "fix", "name", "status"], "{check}");
    }
    let expected = if host_synced() == Some(false) {
        "warn"
    } else {
        "pass"
    };
    assert_eq!(map["checks_status"], expected, "{stdout}");
    assert!(!stdout.lines().any(|l| l.starts_with("PASS")), "{stdout}");
}

#[test]
fn json_without_a_receiver_has_only_checks() {
    let (ntp, _h) = fake_sntp(0.0);
    let out = manta()
        .args([
            "doctor",
            "--duration",
            "3",
            "--json",
            "--source",
            "/nonexistent.wav",
        ])
        .arg("--ntp-server")
        .arg(format!("127.0.0.1:{ntp}"))
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let map = json_object(&stdout);
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["checks", "checks_status"], "{stdout}");
    assert_eq!(map["checks_status"], "fail");
}

#[test]
fn no_server_and_no_uplink_are_single_skip_rows() {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 4);
    let cfg = dir.path().join("manta.toml");
    std::fs::write(
        &cfg,
        format!(
            "[input]\ntype = \"file\"\npath = \"{}\"\n",
            wav.display().to_string().replace('\\', "\\\\")
        ),
    )
    .unwrap();
    let (ntp, _h) = fake_sntp(0.0);
    let out = manta()
        .args(["doctor", "--duration", "3", "--config"])
        .arg(&cfg)
        .arg("--ntp-server")
        .arg(format!("127.0.0.1:{ntp}"))
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("SKIP  ports: no [server] table, so manta run starts no servers\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("SKIP  rbn uplink: no [[rbn_uplink]] block to check\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("telnet port"), "{stdout}");
}
