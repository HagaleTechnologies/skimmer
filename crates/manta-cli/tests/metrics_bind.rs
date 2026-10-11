//! MAN-132 scenario 1 acceptance against the real `manta run` binary: the
//! metrics listener (`GET /metrics`, `GET /healthz`) binds
//! `[server].metrics_bind_addr`, `127.0.0.1` by default, while telnet and
//! JSON keep binding `bind_addr` (`0.0.0.0` by default, D14).
//!
//! Helpers are copied from `startup_banner.rs` and
//! `node_health_acceptance.rs`; the repo's test files are self-contained.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
// Used only by the unix-gated reachability test at the bottom of this file.
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
#[cfg(unix)]
use std::process::{Child, Stdio};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::Duration;

/// `seconds` of 48 kHz mono silence. The banner is emitted before the
/// decode loop starts, so the content is irrelevant; 3 s clears
/// `manta_engine`'s two-second calibration window, so a banner-only run
/// exits 0.
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

/// A `[server]` table with ephemeral ports, no status line, and `extra`
/// lines (address keys) appended.
fn write_config(dir: &Path, extra: &str) -> PathBuf {
    let path = dir.join("server.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\nstation_callsign = \"W3XYZ\"\ntelnet_port = 0\njson_port = 0\n\
             status_interval_secs = 0\n{extra}"
        ),
    )
    .unwrap();
    path
}

/// The scenario 1 config: no `bind_addr`, no `metrics_bind_addr`.
const NO_ADDRESS_KEYS: &str = "metrics_port = 0\n";

fn run_command(wav: &Path, cfg: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    cmd.args([
        "run",
        "--source",
        wav.to_str().unwrap(),
        "--dial-freq-hz",
        "14060000",
        "--config",
        cfg.to_str().unwrap(),
    ])
    .env("RUST_LOG", "info");
    cmd
}

fn banner_of(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    stderr
        .lines()
        .find(|l| l.contains("manta ") && l.contains("listening:"))
        .unwrap_or_else(|| panic!("no startup banner in stderr: {stderr}"))
        .to_string()
}

fn assert_banner(banner: &str, present: &[&str], absent: &[&str]) {
    for needle in present {
        assert!(
            banner.contains(needle),
            "{needle:?} missing from {banner:?}"
        );
    }
    for needle in absent {
        assert!(!banner.contains(needle), "{needle:?} in {banner:?}");
    }
}

/// Ticket scenario 1: with no explicit metrics bind address, metrics binds
/// 127.0.0.1, not 0.0.0.0, while telnet and JSON stay on every interface.
#[test]
fn metrics_listens_on_loopback_when_no_metrics_bind_addr_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 3);
    let cfg = write_config(dir.path(), NO_ADDRESS_KEYS);

    let out = run_command(&wav, &cfg).output().unwrap();
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_banner(
        &banner_of(&out),
        &["telnet=0.0.0.0:", "json=0.0.0.0:", "metrics=127.0.0.1:"],
        &["metrics=0.0.0.0:"],
    );
}

#[test]
fn metrics_bind_addr_moves_only_the_metrics_listener() {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 3);
    let cfg = write_config(
        dir.path(),
        "metrics_port = 0\nbind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"0.0.0.0\"\n",
    );

    let out = run_command(&wav, &cfg).output().unwrap();
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_banner(
        &banner_of(&out),
        &["telnet=127.0.0.1:", "json=127.0.0.1:", "metrics=0.0.0.0:"],
        &[],
    );
}

#[test]
fn metrics_bind_addr_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 3);
    let cfg = write_config(dir.path(), NO_ADDRESS_KEYS);

    let out = run_command(&wav, &cfg)
        .env("MANTA_SERVER_METRICS_BIND_ADDR", "0.0.0.0")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_banner(&banner_of(&out), &["metrics=0.0.0.0:"], &[]);
}

#[test]
fn a_failed_bind_names_the_listener_and_its_address() {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 3);
    // Held for the whole run, so the daemon's metrics bind must fail.
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let cfg = write_config(
        dir.path(),
        &format!("bind_addr = \"127.0.0.1\"\nmetrics_port = {port}\n"),
    );

    let out = run_command(&wav, &cfg).output().unwrap();
    drop(taken);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a held port must fail the run: {stderr}"
    );
    let want = format!(
        "binding the metrics server (metrics_bind_addr = \"127.0.0.1\", metrics_port = {port})"
    );
    assert!(stderr.contains(&want), "{want:?} missing from {stderr}");
    assert!(
        !stderr.contains("listening:"),
        "a failed bind must produce no banner: {stderr}"
    );
}

/// MAN-131 scenario 2: whichever of the three listeners hits a port already
/// in use, the error names that listener, its address key and its port.
/// Pins behaviour MAN-132 already delivered (its D8); before MAN-131 only
/// the metrics listener was covered.
#[test]
fn every_failed_bind_names_its_listener_address_and_port() {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 3);
    for (listener, port_key, addr_key) in [
        ("telnet", "telnet_port", "bind_addr"),
        ("JSON", "json_port", "bind_addr"),
        ("metrics", "metrics_port", "metrics_bind_addr"),
    ] {
        // Held for the whole run, so this listener's bind must fail.
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        // Only the listener under test gets the held port; the others stay
        // ephemeral.
        let ports: String = ["telnet_port", "json_port", "metrics_port"]
            .iter()
            .map(|key| {
                let value = if *key == port_key { port } else { 0 };
                format!("{key} = {value}\n")
            })
            .collect();
        let cfg = dir.path().join(format!("{port_key}.toml"));
        std::fs::write(
            &cfg,
            format!(
                "[server]\nstation_callsign = \"W3XYZ\"\nstatus_interval_secs = 0\n\
                 bind_addr = \"127.0.0.1\"\nmetrics_bind_addr = \"127.0.0.1\"\n{ports}"
            ),
        )
        .unwrap();

        let out = run_command(&wav, &cfg).output().unwrap();
        drop(taken);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{listener}: {stderr}");
        for needle in [
            format!("binding the {listener} server"),
            format!("{addr_key} = \"127.0.0.1\""),
            format!("{port_key} = {port}"),
        ] {
            assert!(
                stderr.contains(&needle),
                "{listener}: {needle:?} missing from {stderr}"
            );
        }
        assert!(
            !stderr.contains("listening:"),
            "{listener}: banner after a failed bind: {stderr}"
        );
    }
}

/// Kills and reaps the daemon child process on drop, including on a
/// failing assertion, so a failing assert never leaks a `manta` process.
#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// This machine's IPv4 address on the default route, or `None` when there
/// is none. A UDP `connect` only picks a route; it sends nothing.
#[cfg(unix)]
fn non_loopback_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?; // TEST-NET-1
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Scenario 1 as behaviour, not just the bound address: by default another
/// machine cannot reach metrics, while telnet on the same address can be
/// reached. Gated to unix: Windows retries a refused SYN for seconds, and
/// the banner tests above already cover every OS.
#[cfg(unix)]
#[test]
fn metrics_is_unreachable_on_a_non_loopback_address_by_default() {
    let Some(ip) = non_loopback_ipv4() else {
        eprintln!("skipping: no non-loopback IPv4 address");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // Long enough that the daemon outlives the probes, which take
    // milliseconds once the banner is out.
    let wav = silent_48k_wav(dir.path(), 300);
    let cfg = write_config(dir.path(), NO_ADDRESS_KEYS);

    let mut child = run_command(&wav, &cfg)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let _guard = ChildGuard(child);

    // Keep draining stderr for the child's whole life so it never blocks
    // on a full pipe; hand the banner back over a channel.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("listening:") {
                let _ = tx.send(line);
            }
        }
    });
    let banner = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("no startup banner within 60 s");

    let port_of = |name: &str| -> u16 {
        let re = regex::Regex::new(&format!(r"\b{name}=([0-9.]+):([0-9]+)")).unwrap();
        let c = re
            .captures(&banner)
            .unwrap_or_else(|| panic!("no {name}= in {banner:?}"));
        c[2].parse().unwrap()
    };
    let (telnet_port, metrics_port) = (port_of("telnet"), port_of("metrics"));
    let timeout = Duration::from_secs(3);

    // 1. Positive control: the daemon is alive and the wildcard telnet
    //    listener answers on this address.
    TcpStream::connect_timeout(&SocketAddr::new(ip.into(), telnet_port), timeout)
        .unwrap_or_else(|e| panic!("telnet on {ip}:{telnet_port} must accept: {e}"));

    // 2. Metrics on the same address is refused.
    let err = TcpStream::connect_timeout(&SocketAddr::new(ip.into(), metrics_port), timeout)
        .err()
        .unwrap_or_else(|| panic!("metrics on {ip}:{metrics_port} must refuse by default"));
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "metrics on {ip}:{metrics_port}: {err}"
    );

    // 3. Metrics on loopback answers, so the daemon was still up for (2).
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv4Addr::LOCALHOST.into(), metrics_port),
        timeout,
    )
    .unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut head = [0u8; 9];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"HTTP/1.1 ", "loopback /healthz must answer HTTP");
}
