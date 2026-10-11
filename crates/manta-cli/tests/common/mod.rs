//! Helpers shared by the tests that drive a real `manta` process and its
//! signals (`signal_shutdown.rs`, MAN-85; `config_reload.rs`, MAN-78).
// Each test binary compiles this module separately and uses only part of it.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Replay seconds in the fixture. Only has to outlast the test window --
/// every caller turns "too short for this machine" into an explicit
/// failure rather than a false pass.
pub const FIXTURE_SECONDS: f64 = 300.0;

/// One 300 s fixture for every test binary: ~115 MB and a full render, not
/// worth paying more than once. Written under `CARGO_TARGET_TMPDIR` (cleaned
/// by `cargo clean`) rather than a `tempfile::TempDir`: Rust never runs
/// destructors for statics, so a shared `TempDir` here would leak 115 MB
/// into the system temp directory on every run. Rendered to a
/// process-unique name and atomically renamed into place, so two
/// concurrent `cargo test` invocations (or two test binaries) cannot
/// observe a half-written WAV.
pub fn fixture_wav() -> &'static Path {
    static WAV: OnceLock<PathBuf> = OnceLock::new();
    WAV.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("man85-signal-fixture");
        let wav = root.join("v1.wav");
        if wav.exists() {
            return wav;
        }
        let staging = root.with_extension(format!("staging.{}", std::process::id()));
        std::fs::create_dir_all(&staging).unwrap();
        let spec = manta_testkit::vectors::VectorSpec {
            fs: 48_000.0,
            duration_s: FIXTURE_SECONDS,
            ..manta_testkit::vectors::v1()
        };
        manta_testkit::vectors::write_fixture_set(&spec, &staging).unwrap();
        // Loser of a rename race: another process already published one.
        if std::fs::rename(&staging, &root).is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        assert!(
            wav.exists(),
            "fixture missing after publish: {}",
            wav.display()
        );
        wav
    })
    .as_path()
}

pub fn wait_bounded(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if start.elapsed() > budget {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `tracing` writes ANSI colour codes even into a pipe; structured fields
/// (`blocklist_calls=1`) only match once they are stripped.
pub fn strip_ansi(line: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap())
        .replace_all(line, "")
        .into_owned()
}

/// Forwards every line `reader` yields, ANSI-stripped, over a channel from
/// a background thread, and keeps draining to EOF so the child never hits
/// `EPIPE` on a later write (`eprintln!` panics on a failed write). The
/// channel disconnects at EOF -- the child closed its end, i.e. exited.
pub fn line_channel(reader: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        // `read_until`, not `lines()`: a telnet peer may send non-UTF-8
        // bytes (IAC), which must not end the stream early.
        while matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            let line = String::from_utf8_lossy(&buf);
            // A dropped receiver just means nobody is listening any more;
            // keep draining.
            let _ = tx.send(strip_ansi(line.trim_end()));
            buf.clear();
        }
    });
    rx
}

/// Waits up to `timeout` for a line containing every one of `needles`, and
/// returns it. Panics naming what was missing; a disconnected channel means
/// the stream ended first (the process exited, or the socket closed).
pub fn expect_line(rx: &Receiver<String>, needles: &[&str], timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                if needles.iter().all(|n| line.contains(n)) {
                    return line;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                panic!("no line containing {needles:?} within {timeout:?}")
            }
            Err(RecvTimeoutError::Disconnected) => panic!(
                "the stream ended before a line containing {needles:?} -- if the daemon \
                 exited, the fixture ended before it; raise FIXTURE_SECONDS"
            ),
        }
    }
}
