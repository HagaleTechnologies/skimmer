//! MAN-85: SIGTERM (and SIGHUP) must enter the same graceful-shutdown drain
//! path SIGINT already uses, and the process must exit 0 rather than with a
//! signal-derived code.
//!
//! Since MAN-78, SIGTERM and SIGHUP go through `signal-hook` on Unix (ctrlc
//! keeps SIGINT), and these tests pin that: drop the SIGTERM registration
//! and `sigterm_exits_zero_through_the_drain_path` goes red. SIGHUP drains
//! only without a `[server]` table; `config_reload.rs` pins the daemon-mode
//! reload.
#![cfg(unix)]

mod common;

use common::{fixture_wav, wait_bounded};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The LAST line `manta listen` prints at startup, and therefore the point
/// at which every startup step -- `ctrlc::set_handler` included -- has
/// certainly completed. The tests below wait for it so they can attribute a
/// nonzero exit to a missing drain rather than to a signal that merely
/// arrived before the handler existed.
///
/// As of MAN-122 review round 6 the handler is installed considerably
/// earlier than this line (before `start_spot_server` and its `listening:`
/// banner), so waiting here is now conservative rather than necessary --
/// deliberately so: it keeps these tests measuring the drain and nothing
/// else. `a_signal_at_the_startup_banner_still_exits_through_the_drain`
/// below is the test that pins the earlier boundary.
const READY_MARKER: &str = "manta: listening;";

/// Generous upper bound on signal -> exit. The real number here is ~20 ms
/// (no clients connected, so `tasks::await_all` returns immediately); this
/// only has to be far below the fixture's natural EOF.
const EXIT_BUDGET: Duration = Duration::from_secs(15);

fn spawn_listen() -> Child {
    Command::new(env!("CARGO_BIN_EXE_manta"))
        .arg("listen")
        .arg("--source")
        .arg(fixture_wav())
        .arg("--json")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Blocks until the child has registered its signal handler.
fn await_ready(child: &mut Child) {
    let stderr = child.stderr.take().expect("stderr piped");
    for line in BufReader::new(stderr).lines() {
        if line.unwrap().contains(READY_MARKER) {
            return;
        }
    }
    panic!("manta exited before printing {READY_MARKER:?}");
}

fn assert_signal_drains_and_exits_zero(sig: libc::c_int, name: &str) {
    // `control` is the same command, never signalled. It is what makes the
    // assertion below mean something: without it, a fixture short enough to
    // hit EOF inside the test window would exit 0 on its own and this test
    // would pass even with SIGTERM completely unhandled.
    let mut control = spawn_listen();
    let mut child = spawn_listen();
    await_ready(&mut control);
    await_ready(&mut child);

    assert!(
        child.try_wait().unwrap().is_none(),
        "manta exited on its own before {name} was sent"
    );
    let sent = Instant::now();
    assert_eq!(
        unsafe { libc::kill(child.id() as i32, sig) },
        0,
        "kill({name}) failed: {}",
        std::io::Error::last_os_error()
    );
    let status = wait_bounded(&mut child, EXIT_BUDGET)
        .unwrap_or_else(|| panic!("{name} did not stop manta within {EXIT_BUDGET:?}"));
    let elapsed = sent.elapsed();

    let control_still_running = control.try_wait().unwrap().is_none();
    let _ = control.kill();
    let _ = control.wait();
    assert!(
        control_still_running,
        "an unsignalled run of the same fixture also finished within {elapsed:?}, so this \
         test cannot attribute the exit to {name} -- raise FIXTURE_SECONDS"
    );

    assert_eq!(
        status.code(),
        Some(0),
        "{name} must run the shutdown drain and exit 0, got {status:?} after {elapsed:?}"
    );
}

#[test]
fn sigterm_exits_zero_through_the_drain_path() {
    assert_signal_drains_and_exits_zero(libc::SIGTERM, "SIGTERM");
}

#[test]
fn sigint_exits_zero_through_the_drain_path() {
    assert_signal_drains_and_exits_zero(libc::SIGINT, "SIGINT");
}

#[test]
fn sighup_exits_zero_through_the_drain_path() {
    // Not in MAN-85's Gherkin. Since MAN-78, SIGHUP still drains and exits
    // *without* a `[server]` table (a foreground run with no clients,
    // history or uplink to protect, where a terminal hangup should end it);
    // with one it reloads instead, which `config_reload.rs` pins.
    assert_signal_drains_and_exits_zero(libc::SIGHUP, "SIGHUP");
}

/// The `STOPSIGNAL SIGINT` workaround this ticket removes must not come
/// back: with SIGTERM handled, retargeting the container stop signal only
/// hides whether the real path works.
#[test]
fn dockerfile_does_not_retarget_the_container_stop_signal() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Dockerfile");
    let text = std::fs::read_to_string(&path).unwrap();
    let directive = text
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with("STOPSIGNAL"));
    assert!(
        directive.is_none(),
        "manta handles SIGTERM natively since MAN-85; the image must use Docker's default \
         stop signal, found: {directive:?}"
    );
}

/// MAN-122 review round 6 (P2). The tests above wait for `READY_MARKER`,
/// which is deliberately the LAST line the daemon prints at startup -- so
/// they cannot see the window this test exists for. With `--config`, the
/// daemon first logs a `listening:` banner naming its bound telnet/JSON/
/// metrics addresses, strictly earlier than that marker. The banner is an
/// *advertisement*: it is the earliest instant at which a supervisor or an
/// operator can read a real address and react, and the reaction under test
/// is "stop the daemon immediately". Before this round `ctrlc::set_handler`
/// ran only AFTER `start_spot_server` returned, so a signal landing here
/// retained its default disposition and killed the process outright,
/// skipping the client/status drain -- with `SIGTERM`'s default kill giving
/// exit code 143, not 0. Signalling at the banner rather than at the marker
/// is the whole point of this test; moving the wait to `READY_MARKER` would
/// make it a duplicate of `sigterm_exits_zero_through_the_drain_path`.
#[test]
fn a_signal_at_the_startup_banner_still_exits_through_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("server.toml");
    // Port 0 everywhere: this file may run concurrently with anything else
    // in the workspace, and a fixed port would make it flaky rather than
    // meaningful. `status_interval_secs = 0` disables the periodic status
    // task, which has no bearing on the ordering under test.
    std::fs::write(
        &cfg_path,
        r#"
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

    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["run", "--source"])
            .arg(fixture_wav())
            .args([
                "--dial-freq-hz",
                "14060000",
                "--config",
                cfg_path.to_str().unwrap(),
                "--json",
            ])
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };

    // Same control rationale as `assert_signal_drains_and_exits_zero`: a
    // fixture that reached EOF inside the test window would exit 0 on its
    // own and pass this test with the handler installed too late.
    let mut control = spawn();
    let mut child = spawn();
    let control_drain = drain_stderr(&mut control);
    // `listening:` (colon) is the banner; `READY_MARKER` is
    // `manta: listening;` (semicolon), so this match cannot accidentally
    // wait for the later marker.
    let child_drain = await_banner(&mut child);

    assert!(
        child.try_wait().unwrap().is_none(),
        "manta exited on its own before it could be signalled at the banner"
    );
    let sent = Instant::now();
    assert_eq!(
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) },
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
    let status = wait_bounded(&mut child, EXIT_BUDGET).unwrap_or_else(|| {
        panic!("SIGTERM at the banner did not stop manta within {EXIT_BUDGET:?}")
    });
    let elapsed = sent.elapsed();
    let _ = child_drain.join();

    let control_still_running = control.try_wait().unwrap().is_none();
    let _ = control.kill();
    let _ = control.wait();
    let _ = control_drain.join();
    assert!(
        control_still_running,
        "an unsignalled run of the same fixture also finished within {elapsed:?}, so this \
         test cannot attribute the exit to SIGTERM -- raise FIXTURE_SECONDS"
    );

    assert_eq!(
        status.code(),
        Some(0),
        "a SIGTERM delivered as soon as the daemon advertised its bound addresses must still \
         run the shutdown drain and exit 0, got {status:?} after {elapsed:?}"
    );
}

/// Blocks until the child logs its `listening:` startup banner, then keeps
/// draining stderr on a background thread.
///
/// The draining is load-bearing, not tidiness: the banner is an EARLY line,
/// so the daemon still has its `READY_MARKER` `eprintln!` and its readiness
/// event to write afterwards. Dropping the read end here would make those
/// writes fail with `EPIPE` -- and `eprintln!` panics on a failed write,
/// which would abort the child with a nonzero code and turn this into a
/// test of nothing. `await_ready` above can drop its reader safely only
/// because it waits for the last startup line.
fn await_banner(child: &mut Child) -> std::thread::JoinHandle<()> {
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr piped"));
    let mut line = String::new();
    let mut saw_banner = false;
    while stderr.read_line(&mut line).unwrap() > 0 {
        if line.contains("listening:") {
            saw_banner = true;
            break;
        }
        line.clear();
    }
    assert!(
        saw_banner,
        "manta exited before logging its `listening:` startup banner"
    );
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    })
}

/// Drains a child's piped stderr to EOF on a background thread, for a child
/// whose output this test never inspects. Same `EPIPE` reasoning as
/// `await_banner`.
fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<()> {
    let mut stderr = child.stderr.take().expect("stderr piped");
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    })
}
