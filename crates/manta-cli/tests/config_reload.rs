//! MAN-78 acceptance scenarios, against a real daemon process:
//!
//!   Scenario: A reload picks up an updated blocklist without dropping clients
//!     Given manta is running with connected telnet clients
//!     When an operator edits the blocklist file and sends SIGHUP
//!     Then the new blocklist takes effect
//!     And connected clients and the sh/dx history are not dropped
//!
//!   Scenario: An invalid reload is rejected without disrupting the running daemon
//!     Given a reload is triggered with an invalid list file
//!     When manta processes the reload
//!     Then it logs the error and keeps running on the previous configuration
//!
//! The fixture (`common::fixture_wav`, v1: "CQ CQ DE W1AW W1AW K" on a loop)
//! makes W1AW spottable every few seconds of audio, and file replay is not
//! paced, so the test window is the replay's wall time after `ready:
//! decoding` (~5 s on a fast machine; slower machines only widen it).
//! Starting with W1AW blocklisted means a W1AW `DX de` line can only appear
//! once a reload has replaced the blocklist.
#![cfg(unix)]

mod common;

use common::{expect_line, fixture_wav, line_channel, wait_bounded};
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Signal -> exit. ~20 ms in practice; only has to be far below the
/// fixture's natural EOF.
const EXIT_BUDGET: Duration = Duration::from_secs(15);
/// Startup (banner, telnet login, first batch) and each expected line.
const LINE_TIMEOUT: Duration = Duration::from_secs(60);

fn server_toml(extra_server: &str, uplink: &str) -> String {
    format!(
        "[server]\n\
         station_callsign = \"W3XYZ\"\n\
         bind_addr = \"127.0.0.1\"\n\
         metrics_bind_addr = \"127.0.0.1\"\n\
         telnet_port = 0\n\
         json_port = 0\n\
         metrics_port = 0\n\
         status_interval_secs = 0\n\
         {extra_server}\n\
         [spot]\n\
         blocklist_path = \"bl.txt\"\n\
         {uplink}"
    )
}

fn uplink_toml(port: u16, dry_run: bool) -> String {
    format!(
        "\n[[rbn_uplink]]\nenabled = true\ntarget_host = \"127.0.0.1\"\n\
         target_port = {port}\nlogin_callsign = \"W3XYZ\"\ndry_run = {dry_run}\n"
    )
}

struct Daemon {
    child: Child,
    stderr: Receiver<String>,
    telnet: TcpStream,
    telnet_lines: Receiver<String>,
}

impl Daemon {
    /// Starts `manta run --config <dir>/manta.toml` on the shared fixture,
    /// logs a telnet client in, and returns once the pipeline is decoding.
    fn start(dir: &Path) -> Daemon {
        let mut child = Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["run", "--source"])
            .arg(fixture_wav())
            .args(["--dial-freq-hz", "14060000", "--config"])
            .arg(dir.join("manta.toml"))
            .arg("--json")
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = line_channel(child.stderr.take().unwrap());

        let banner = expect_line(&stderr, &["listening:", "telnet=127.0.0.1:"], LINE_TIMEOUT);
        let port: u16 = banner
            .split("telnet=127.0.0.1:")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("no telnet port in {banner:?}"));
        let mut telnet = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let telnet_lines = line_channel(telnet.try_clone().unwrap());
        expect_line(&telnet_lines, &["Please enter your callsign"], LINE_TIMEOUT);
        telnet.write_all(b"W3ABC\r\n").unwrap();

        // A daemon advertises the reload in its readiness line.
        expect_line(
            &stderr,
            &[
                "manta: listening;",
                "SIGHUP to reload [spot] lists and dry_run",
            ],
            LINE_TIMEOUT,
        );
        expect_line(&stderr, &["ready: decoding"], LINE_TIMEOUT);
        Daemon {
            child,
            stderr,
            telnet,
            telnet_lines,
        }
    }

    fn signal(&self, sig: libc::c_int) {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, sig) },
            0,
            "kill({sig}) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    fn assert_still_running(&mut self, during: &str) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "manta exited {during} -- if no signal stopped it, the fixture ended; raise \
             FIXTURE_SECONDS"
        );
    }

    /// Discards every telnet line received so far, and checks the socket is
    /// still open.
    fn drain_telnet(&self) {
        std::thread::sleep(Duration::from_millis(200));
        loop {
            match self.telnet_lines.try_recv() {
                Ok(_) => continue,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => panic!("the telnet client was disconnected"),
            }
        }
    }

    /// Asserts no `DX de ... W1AW` line arrives for `window`, with the
    /// daemon running and the socket open throughout.
    fn assert_no_w1aw_spot_for(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.telnet_lines.recv_timeout(left) {
                Ok(line) => assert!(
                    !(line.contains("DX de") && line.contains("W1AW")),
                    "W1AW was spotted while still blocklisted: {line:?}"
                ),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the telnet client was disconnected")
                }
            }
        }
        self.assert_still_running("while W1AW was meant to stay blocklisted");
    }

    /// SIGTERM, then a clean drain and exit 0.
    fn stop(mut self) {
        self.assert_still_running("before SIGTERM");
        self.signal(libc::SIGTERM);
        let status = wait_bounded(&mut self.child, EXIT_BUDGET)
            .unwrap_or_else(|| panic!("SIGTERM did not stop manta within {EXIT_BUDGET:?}"));
        assert_eq!(status.code(), Some(0), "{status:?}");
    }
}

#[test]
fn sighup_applies_a_new_blocklist_without_dropping_clients_or_history() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("manta.toml");
    let blocklist = dir.path().join("bl.txt");
    std::fs::write(&config, server_toml("", "")).unwrap();
    std::fs::write(&blocklist, "W1AW\n").unwrap();
    let mut daemon = Daemon::start(dir.path());

    // The edit + SIGHUP under test.
    std::fs::write(&blocklist, "K1BAD\n").unwrap();
    daemon.signal(libc::SIGHUP);
    expect_line(
        &daemon.stderr,
        &["reload: applied", "blocklist_calls=1"],
        LINE_TIMEOUT,
    );
    // The new blocklist took effect end to end: W1AW, blocked at startup,
    // now reaches the connected client.
    expect_line(&daemon.telnet_lines, &["DX de", "W1AW"], LINE_TIMEOUT);
    daemon.assert_still_running("after the first reload");

    // A second reload that also changes a restart-only key: the lists
    // still apply (W1AW blocked again), and the key is named.
    std::fs::write(&blocklist, "W1AW\n").unwrap();
    let cty = dir.path().join("cty.dat");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../manta-spot/data/cty.dat"),
        &cty,
    )
    .unwrap();
    std::fs::write(
        &config,
        server_toml(
            "operator_qth = \"Testville\"",
            &format!("cty_path = \"{}\"\n", cty.display()),
        ),
    )
    .unwrap();
    daemon.signal(libc::SIGHUP);
    expect_line(&daemon.stderr, &["reload: applied"], LINE_TIMEOUT);
    expect_line(
        &daemon.stderr,
        &["WARN", "restart", "server.operator_qth", "spot.cty_path"],
        LINE_TIMEOUT,
    );

    // Same socket: the client was never dropped, and the history survived
    // both reloads. W1AW is blocklisted again, so a W1AW line from here on
    // can only be the `sh/dx` replay.
    daemon.drain_telnet();
    daemon.telnet.write_all(b"sh/dx\r\n").unwrap();
    expect_line(&daemon.telnet_lines, &["DX de", "W1AW"], LINE_TIMEOUT);
    if let Err(TryRecvError::Disconnected) = daemon.telnet_lines.try_recv() {
        panic!("the telnet client was disconnected");
    }

    daemon.stop();
}

#[test]
fn an_invalid_reload_is_logged_and_the_previous_lists_stay() {
    // A target that refuses connections: the uplink only has to exist for
    // its `dry_run` flag to be reloaded.
    let uplink_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("manta.toml");
    let blocklist = dir.path().join("bl.txt");
    std::fs::write(&config, server_toml("", &uplink_toml(uplink_port, true))).unwrap();
    std::fs::write(&blocklist, "W1AW\n").unwrap();
    let mut daemon = Daemon::start(dir.path());

    // A UTF-16 (Notepad-style) blocklist.
    std::fs::write(&blocklist, b"\xff\xfeK\x001\x00B\x00A\x00D\x00\n\x00").unwrap();
    daemon.signal(libc::SIGHUP);
    expect_line(
        &daemon.stderr,
        &[
            "ERROR",
            "reload: rejected; still running the previous configuration",
            "reading blocklist file",
            &blocklist.display().to_string(),
            "valid UTF-8",
        ],
        LINE_TIMEOUT,
    );

    // A broken config file.
    std::fs::write(&config, "[spot\n").unwrap();
    daemon.signal(libc::SIGHUP);
    expect_line(
        &daemon.stderr,
        &["reload: rejected", "parsing config file"],
        LINE_TIMEOUT,
    );

    // The previous blocklist (W1AW) still holds, and nothing was dropped.
    daemon.assert_no_w1aw_spot_for(Duration::from_millis(1500));

    // Positive control: a valid reload after the rejections still applies,
    // lists and dry_run alike.
    std::fs::write(&config, server_toml("", &uplink_toml(uplink_port, false))).unwrap();
    std::fs::write(&blocklist, "K1BAD\n").unwrap();
    daemon.signal(libc::SIGHUP);
    expect_line(
        &daemon.stderr,
        &["reload: applied", "blocklist_calls=1"],
        LINE_TIMEOUT,
    );
    expect_line(
        &daemon.stderr,
        &[
            "WARN",
            "dry_run = false -- transmitting real spots",
            &format!("target=127.0.0.1:{uplink_port}"),
        ],
        LINE_TIMEOUT,
    );
    expect_line(&daemon.telnet_lines, &["DX de", "W1AW"], LINE_TIMEOUT);

    daemon.stop();
}

/// Kills and reaps the daemon on drop, so a failed assertion never leaves it
/// reconnecting to a fake KiwiSDR that has gone away.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Plays a KiwiSDR just far enough for `KiwiIqSource::connect`
/// (crates/manta-input/src/kiwi.rs) to return, then sends only pings, every
/// 200 ms, until `done` or the client hangs up. No SND frame ever arrives, so
/// the daemon's decode thread waits in the socket `recv`, and each ping
/// resets the source's stall bound.
fn fake_kiwi(listener: TcpListener, done: Arc<AtomicBool>) -> Result<(), String> {
    use tungstenite::Message;

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + LINE_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if done.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    return Err("manta never connected to the fake KiwiSDR".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("accept: {e}")),
        }
    };
    // BSD/macOS sockets inherit the listener's non-blocking mode.
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut ws = tungstenite::accept(stream).map_err(|e| format!("WebSocket handshake: {e}"))?;
    ws.read().map_err(|e| format!("reading SET auth: {e}"))?;
    // 12000 Hz / 93.75 Hz per channel = 128 channels: a valid rate.
    ws.send(Message::binary(b"MSG sample_rate=12000.000".to_vec()))
        .map_err(|e| format!("sending sample_rate: {e}"))?;
    while !done.load(Ordering::Relaxed) {
        if ws.send(Message::Ping(Default::default())).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Validate attempt 1's review finding: a reload must not cost a socket
/// source its connection. Linux delivered SIGHUP to the decode (main)
/// thread while it waited in the KiwiSDR source's `recv`, and on a socket
/// with a read timeout that `recv` fails with EINTR even under `SA_RESTART`
/// (signal(7)); the source took the error for a lost link and
/// `ReconnectingSource` dropped and reopened it (`source kiwi lost: ...`).
#[test]
fn sighup_does_not_drop_a_kiwisdr_source() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let kiwi_port = listener.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("manta.toml");
    std::fs::write(
        &config,
        format!(
            "{}\n[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = {kiwi_port}\n\
             freq_hz = 14025000.0\n",
            server_toml("", "")
        ),
    )
    .unwrap();
    std::fs::write(dir.path().join("bl.txt"), "W1AW\n").unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let fake = std::thread::spawn({
        let done = done.clone();
        move || fake_kiwi(listener, done)
    });

    let mut daemon = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["run", "--config"])
            .arg(&config)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = line_channel(daemon.0.stderr.take().unwrap());
    expect_line(
        &stderr,
        &["manta: listening;", "SIGHUP to reload"],
        LINE_TIMEOUT,
    );
    // The readiness line directly precedes the decode loop's first read.
    std::thread::sleep(Duration::from_millis(500));

    // Every stderr line from the first SIGHUP until 1 s after the second
    // reload applied: the source's loss line, if any, comes from the
    // decode thread and can precede the reload thread's `applied`.
    let mut seen = Vec::new();
    for _ in 0..2 {
        assert_eq!(
            unsafe { libc::kill(daemon.0.id() as i32, libc::SIGHUP) },
            0,
            "kill(SIGHUP) failed: {}",
            std::io::Error::last_os_error()
        );
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = stderr
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("no `reload: applied` ({e}); stderr so far: {seen:#?}"));
            let applied = line.contains("reload: applied");
            seen.push(line);
            if applied {
                break;
            }
        }
        // Back in the source's `recv` before the next signal.
        std::thread::sleep(Duration::from_millis(300));
    }
    let settle = Instant::now() + Duration::from_secs(1);
    while let Some(left) = settle.checked_duration_since(Instant::now()) {
        match stderr.recv_timeout(left) {
            Ok(line) => seen.push(line),
            Err(_) => break,
        }
    }
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "manta exited after a reload; stderr: {seen:#?}"
    );
    assert!(
        !seen.iter().any(|line| line.contains("lost:")),
        "a SIGHUP reload dropped the KiwiSDR source; stderr: {seen:#?}"
    );

    done.store(true, Ordering::Relaxed);
    drop(daemon);
    fake.join().unwrap().unwrap();
}
