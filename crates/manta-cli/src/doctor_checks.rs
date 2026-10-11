//! `manta doctor`'s setup checks: config, audio, server ports, clock, RBN
//! uplink reachability, and the receiver and signal stages, each one named
//! line with a fix for every problem. Every check is a pure classifier plus
//! a thin I/O shell. See
//! docs/DECISIONS/2026-10-10-man126-doctor-setup-checks.md.

use crate::clock_check::{self, NtpServer};
use crate::config::Loaded;
use crate::config_cmd::{self, Listener};
use manta_server::config::{RbnUplinkConfig, ServerConfig};
use manta_server::status_doc::escape_for_terminal;
use std::io::{self, Write as _};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

/// One named check. WARN and FAIL always carry a fix: only their
/// constructors take one.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail, None)
    }

    pub fn skip(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Skip, detail, None)
    }

    pub fn warn(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(name, Status::Warn, detail, Some(fix.into()))
    }

    pub fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self::new(name, Status::Fail, detail, Some(fix.into()))
    }

    fn new(
        name: impl Into<String>,
        status: Status,
        detail: impl Into<String>,
        fix: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
            fix,
        }
    }
}

/// One line, plus an indented `fix:` line for WARN and FAIL. Host names,
/// paths, device names and OS errors can carry control characters, so all
/// text is escaped.
pub(crate) fn render(check: &Check) -> String {
    let word = match check.status {
        Status::Pass => "PASS",
        Status::Warn => "WARN",
        Status::Fail => "FAIL",
        Status::Skip => "SKIP",
    };
    let mut out = format!(
        "{word}  {}: {}\n",
        escape_for_terminal(&check.name),
        escape_for_terminal(&check.detail)
    );
    if let (Status::Warn | Status::Fail, Some(fix)) = (check.status, &check.fix) {
        out.push_str(&format!("      fix: {}\n", escape_for_terminal(fix)));
    }
    out
}

/// The closing line: every failed and warned check by name, in check order.
pub(crate) fn summary(checks: &[Check]) -> String {
    let names = |status: Status| -> Vec<String> {
        checks
            .iter()
            .filter(|c| c.status == status)
            .map(|c| escape_for_terminal(&c.name))
            .collect()
    };
    let (failed, warned) = (names(Status::Fail), names(Status::Warn));
    let passed = checks.iter().filter(|c| c.status == Status::Pass).count();
    let skipped = checks.iter().filter(|c| c.status == Status::Skip).count();
    let counts = if skipped > 0 {
        format!("({passed} passed, {skipped} skipped)")
    } else {
        format!("({passed} passed)")
    };
    let warnings = format!(
        "{} warning{}: {}",
        warned.len(),
        if warned.len() == 1 { "" } else { "s" },
        warned.join(", ")
    );
    match (failed.is_empty(), warned.is_empty()) {
        (true, true) => format!("doctor: all checks passed {counts}"),
        (true, false) => format!("doctor: {warnings} {counts}"),
        (false, true) => format!(
            "doctor: {} failed: {} {counts}",
            failed.len(),
            failed.join(", ")
        ),
        (false, false) => format!(
            "doctor: {} failed: {}; {warnings} {counts}",
            failed.len(),
            failed.join(", ")
        ),
    }
}

/// `Fail` if any check failed, else `Warn` if any warned, else `Pass`.
pub(crate) fn worst(checks: &[Check]) -> Status {
    if checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        Status::Warn
    } else {
        Status::Pass
    }
}

/// Where checks go: printed as each result is known (human mode), or only
/// collected (`--json`).
pub(crate) struct Sink {
    json: bool,
    pub checks: Vec<Check>,
}

impl Sink {
    pub fn new(json: bool) -> Self {
        Self {
            json,
            checks: Vec::new(),
        }
    }

    pub fn push(&mut self, check: Check) {
        if !self.json {
            print!("{}", render(&check));
            let _ = io::stdout().flush();
        }
        self.checks.push(check);
    }
}

/// What the setup checks read: the loaded config and the receiver.
pub(crate) struct SetupInputs<'a> {
    pub config_path: Option<&'a Path>,
    pub loaded: &'a Loaded,
    pub needs_dial: bool,
    pub receiver: &'a ReceiverDesc,
    pub ntp_server: &'a NtpServer,
}

/// Every check before the receiver opens, in doctor's fixed order. The
/// network checks (clock and each uplink) run at the same time.
pub(crate) fn run_setup_checks(inputs: &SetupInputs<'_>, sink: &mut Sink) {
    sink.push(config_check(
        inputs.config_path,
        inputs.loaded,
        inputs.needs_dial,
    ));
    sink.push(audio_check(
        inputs.receiver.kind.label(),
        inputs.receiver.kind == ReceiverKind::SoundCard,
        coppa_audio::list_devices,
    ));
    for check in port_checks(inputs.loaded.server.as_ref()) {
        sink.push(check);
    }
    let clock = start_clock_check(inputs.ntp_server.clone(), clock_check::NTP_BUDGET);
    let uplinks = start_uplink_checks(
        &inputs.loaded.rbn_uplink,
        UPLINK_BUDGET,
        resolve_host,
        connect_host,
    );
    sink.push(clock.finish());
    for uplink in uplinks {
        sink.push(uplink.finish());
    }
}

/// `manta run`'s two refusals before any I/O: the example callsign, and a
/// missing dial frequency when `[server]` would start. `MANTA_SERVER_*`
/// alone can build a `[server]` table that `run` checks the same way, so
/// only a run with neither a file nor a `[server]` is skipped.
pub(crate) fn config_check(config_path: Option<&Path>, loaded: &Loaded, needs_dial: bool) -> Check {
    const NAME: &str = "config";
    let path = match config_path {
        Some(path) => path.display().to_string(),
        None if loaded.server.is_some() => "the MANTA_* environment variables".to_string(),
        None => return Check::skip(NAME, "no config file (--config or MANTA_CONFIG)"),
    };
    if let Some(key) = config_cmd::example_callsign_key(loaded) {
        return Check::fail(
            NAME,
            format!(
                "{key} is still the example \"{}\", so manta run refuses to start",
                config_cmd::EXAMPLE_CALLSIGN
            ),
            format!("set {key} to your own callsign in {path}"),
        );
    }
    if needs_dial {
        return Check::fail(
            NAME,
            "this receiver reports no radio frequency and no dial frequency is set, so manta \
             run refuses to start its servers",
            format!(
                "set input.center_freq_hz in {path}, or pass --dial-freq-hz, to the radio's \
                 dial frequency in Hz"
            ),
        );
    }
    Check::pass(NAME, format!("manta run's start-up checks accept {path}"))
}

/// `addr:port`, bracketing an IPv6 literal.
fn host_port(addr: &str, port: u16) -> String {
    if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    }
}

/// One row per `[server]` listener, or one SKIP row with no `[server]`.
/// Each port is bound with `run`'s address/port shape and released at once;
/// another program can still take it before `run` starts.
pub(crate) fn port_checks(server: Option<&ServerConfig>) -> Vec<Check> {
    let Some(server) = server else {
        return vec![Check::skip(
            "ports",
            "no [server] table, so manta run starts no servers",
        )];
    };
    let listeners = config_cmd::server_listeners(server);
    (0..listeners.len())
        .map(|i| {
            let l = &listeners[i];
            let name = format!("{} port", l.name);
            if l.port == 0 {
                return Check::skip(
                    name,
                    "port 0: the system picks a free port when manta run starts",
                );
            }
            if let Some(first) = config_cmd::duplicate_of(&listeners, i) {
                return Check::fail(
                    name,
                    format!(
                        "{} {} is the same as {}, and two servers cannot share a port",
                        l.port_key, l.port, first.port_key
                    ),
                    format!("set a different {} in [server]", l.port_key),
                );
            }
            match std::net::TcpListener::bind((l.addr.as_str(), l.port)) {
                Ok(probe) => {
                    drop(probe);
                    Check::pass(name, format!("{} is free", host_port(&l.addr, l.port)))
                }
                Err(e) => classify_bind_error(l, &e),
            }
        })
        .collect()
}

/// Why a listener could not bind, and what to change.
pub(crate) fn classify_bind_error(l: &Listener, e: &io::Error) -> Check {
    let name = format!("{} port", l.name);
    let at = host_port(&l.addr, l.port);
    match e.kind() {
        io::ErrorKind::AddrInUse => Check::fail(
            name,
            format!("{at} is already in use"),
            format!(
                "stop the program listening on port {port} (on Linux, `sudo ss -ltnp 'sport = \
                 :{port}'` names it) or set a free {key} in [server]; if it is a running manta, \
                 check it with `manta status` instead",
                port = l.port,
                key = l.port_key
            ),
        ),
        io::ErrorKind::AddrNotAvailable => Check::fail(
            name,
            format!("{} is not an address of this machine", l.addr),
            format!(
                "set {} in [server] to one of this machine's addresses, or 0.0.0.0 for all of \
                 them",
                l.addr_key
            ),
        ),
        io::ErrorKind::PermissionDenied => Check::fail(
            name,
            format!("this user may not listen on port {}", l.port),
            "use a port above 1023, or let manta bind low ports (on Linux, \
             AmbientCapabilities=CAP_NET_BIND_SERVICE in the systemd unit)",
        ),
        _ => Check::fail(
            name,
            format!("could not listen on {at}: {e}"),
            format!("check {} and {} in [server]", l.addr_key, l.port_key),
        ),
    }
}

/// The whole per-target cap: the same figure as the daemon's per-address
/// connect timeout.
pub(crate) const UPLINK_BUDGET: Duration = Duration::from_secs(10);
/// One address's connect limit, inside `UPLINK_BUDGET`.
const UPLINK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long past its own budget a network check's thread is waited for;
/// a stalled name lookup cannot be cancelled, so it is abandoned.
const THREAD_GRACE: Duration = Duration::from_secs(1);

/// Why an uplink target could not be reached.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ProbeError {
    Resolve(String),
    Refused(String),
    TimedOut,
    Other(String),
}

pub(crate) type ResolveFn = fn(&str, u16) -> io::Result<Vec<SocketAddr>>;
pub(crate) type ConnectFn = fn(&SocketAddr, Duration) -> io::Result<TcpStream>;

fn resolve_host(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok((host, port).to_socket_addrs()?.collect())
}

fn connect_host(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    TcpStream::connect_timeout(addr, timeout)
}

/// Resolve `host`, then try each address until one accepts, all within
/// `budget`. The accepted connection is dropped at once: nothing is sent.
pub(crate) fn uplink_probe(
    host: &str,
    port: u16,
    budget: Duration,
    resolve: impl Fn(&str, u16) -> io::Result<Vec<SocketAddr>>,
    connect: impl Fn(&SocketAddr, Duration) -> io::Result<TcpStream>,
) -> Result<Duration, ProbeError> {
    let started = Instant::now();
    let addrs = resolve(host, port).map_err(|e| ProbeError::Resolve(e.to_string()))?;
    if addrs.is_empty() {
        return Err(ProbeError::Resolve("no addresses".to_string()));
    }
    let mut last = None;
    for addr in &addrs {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let attempt = Instant::now();
        match connect(addr, remaining.min(UPLINK_CONNECT_TIMEOUT)) {
            Ok(stream) => {
                let took = attempt.elapsed();
                drop(stream);
                return Ok(took);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        None => ProbeError::TimedOut,
        Some(e) => match e.kind() {
            io::ErrorKind::ConnectionRefused => ProbeError::Refused(e.to_string()),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ProbeError::TimedOut,
            _ => ProbeError::Other(e.to_string()),
        },
    })
}

/// The only place uplink detail and fix strings are written.
fn uplink_check(
    name: String,
    host: &str,
    port: u16,
    budget: Duration,
    result: Result<Duration, ProbeError>,
) -> Check {
    match result {
        Ok(took) => Check::pass(
            name,
            format!("accepted a connection in {} ms", took.as_millis()),
        ),
        Err(ProbeError::Resolve(e)) => Check::fail(
            name,
            format!("could not look up {host}: {e}"),
            format!(
                "check target_host in this [[rbn_uplink]] block, and this machine's DNS (try \
                 nslookup {host})"
            ),
        ),
        Err(ProbeError::Refused(e)) => Check::fail(
            name,
            format!("refused the connection ({e})"),
            "check target_port in this [[rbn_uplink]] block; if it is right, the collector is \
             down or not accepting connections, so try again later or ask its operator",
        ),
        Err(ProbeError::TimedOut) => Check::fail(
            name,
            format!("did not answer within {}", clock_check::fmt_budget(budget)),
            format!(
                "a firewall may be dropping outbound TCP to port {port}; check this machine's \
                 and the network's firewall rules, and target_host"
            ),
        ),
        Err(ProbeError::Other(e)) => Check::fail(
            name,
            format!("could not connect: {e}"),
            format!("check this machine's network connection and its route to {host}"),
        ),
    }
}

/// A result being produced on its own thread, waited for until a deadline.
pub(crate) struct Pending<T> {
    rx: mpsc::Receiver<T>,
    deadline: Instant,
}

impl<T: Send + 'static> Pending<T> {
    fn spawn(budget: Duration, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        Self {
            rx,
            deadline: Instant::now() + budget + THREAD_GRACE,
        }
    }

    /// `None` when the thread missed its deadline; it is abandoned.
    fn wait(self) -> Option<T> {
        self.rx
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .ok()
    }
}

/// A `clock` check in flight.
pub(crate) struct PendingClock {
    server: NtpServer,
    budget: Duration,
    pending: Pending<Result<f64, String>>,
}

pub(crate) fn start_clock_check(server: NtpServer, budget: Duration) -> PendingClock {
    let target = server.clone();
    PendingClock {
        server,
        budget,
        pending: Pending::spawn(budget, move || clock_check::query(&target, budget)),
    }
}

impl PendingClock {
    pub fn finish(self) -> Check {
        let offset = self.pending.wait().unwrap_or_else(|| {
            Err(format!(
                "did not answer within {}",
                clock_check::fmt_budget(self.budget)
            ))
        });
        clock_check::classify(
            &self.server.to_string(),
            offset,
            clock_check::kernel_synced(),
        )
    }
}

/// An `rbn uplink` check in flight, or one already decided (skip rows).
pub(crate) enum PendingUplink {
    Done(Check),
    Probing {
        name: String,
        host: String,
        port: u16,
        budget: Duration,
        pending: Pending<Result<Duration, ProbeError>>,
    },
}

impl PendingUplink {
    pub fn finish(self) -> Check {
        match self {
            PendingUplink::Done(check) => check,
            PendingUplink::Probing {
                name,
                host,
                port,
                budget,
                pending,
            } => {
                let result = pending.wait().unwrap_or(Err(ProbeError::TimedOut));
                uplink_check(name, &host, port, budget, result)
            }
        }
    }
}

/// Starts one probe thread per enabled target, labelled as the daemon's
/// metrics label them (`host:port`, `#N` for repeats).
pub(crate) fn start_uplink_checks(
    configs: &[RbnUplinkConfig],
    budget: Duration,
    resolve: ResolveFn,
    connect: ConnectFn,
) -> Vec<PendingUplink> {
    if configs.is_empty() {
        return vec![PendingUplink::Done(Check::skip(
            "rbn uplink",
            "no [[rbn_uplink]] block to check",
        ))];
    }
    let labels = manta_server::uplink::target_labels(configs);
    configs
        .iter()
        .zip(labels)
        .map(|(config, label)| {
            let name = format!("rbn uplink {label}");
            if !config.enabled {
                return PendingUplink::Done(Check::skip(
                    name,
                    "enabled = false, so manta run does not connect to it",
                ));
            }
            let (host, port) = (config.target_host.clone(), config.target_port);
            let target = host.clone();
            PendingUplink::Probing {
                name,
                host,
                port,
                budget,
                pending: Pending::spawn(budget, move || {
                    uplink_probe(&target, port, budget, resolve, connect)
                }),
            }
        })
        .collect()
}

/// Every `rbn uplink` row, probed concurrently, in config order.
#[cfg(test)]
pub(crate) fn uplink_checks(
    configs: &[RbnUplinkConfig],
    budget: Duration,
    resolve: ResolveFn,
    connect: ConnectFn,
) -> Vec<Check> {
    start_uplink_checks(configs, budget, resolve, connect)
        .into_iter()
        .map(PendingUplink::finish)
        .collect()
}

/// The audio library and, for a sound-card receiver only, its inputs. On
/// Linux the process could not have started without `libasound.so.2`.
pub(crate) fn audio_check(
    receiver_kind: &str,
    uses_sound_card: bool,
    list: impl FnOnce() -> anyhow::Result<Vec<coppa_audio::AudioDevice>>,
) -> Check {
    const NAME: &str = "audio";
    let lib = if cfg!(target_os = "linux") {
        "ALSA runtime library is installed; "
    } else {
        ""
    };
    if !uses_sound_card {
        return Check::pass(
            NAME,
            format!("{lib}this {receiver_kind} receiver does not use a sound card"),
        );
    }
    let devices = match list() {
        Ok(devices) => devices,
        Err(e) => {
            let fix = if cfg!(target_os = "linux") {
                "reinstall the ALSA runtime (sudo apt install --reinstall libasound2, or \
                 libasound2t64 on Debian 13) and check that arecord -l works for this user"
            } else {
                "check that this machine's sound settings list the radio's audio interface as \
                 an input"
            };
            return Check::fail(
                NAME,
                format!("the audio system could not list inputs: {e:#}"),
                fix,
            );
        }
    };
    let inputs: Vec<String> = devices
        .iter()
        .filter(|d| d.input_channels > 0)
        .map(|d| format!("\"{}\"", d.name))
        .collect();
    if inputs.is_empty() {
        let fix = if cfg!(target_os = "linux") {
            "connect the radio's audio interface; arecord -l must list it, and the user running \
             manta must be in the audio group"
        } else {
            "connect the radio's audio interface, and check that this machine's sound settings \
             list it as an input"
        };
        return Check::fail(NAME, "the audio system lists no sound-card inputs", fix);
    }
    const SHOWN: usize = 4;
    let mut listed = inputs[..inputs.len().min(SHOWN)].join(", ");
    if inputs.len() > SHOWN {
        listed.push_str(&format!(", and {} more", inputs.len() - SHOWN));
    }
    let noun = if inputs.len() == 1 { "input" } else { "inputs" };
    Check::pass(
        NAME,
        format!("{lib}{} sound-card {noun}: {listed}", inputs.len()),
    )
}

/// The receiver kinds doctor can name a fix for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReceiverKind {
    SoundCard,
    File,
    Kiwi,
    #[cfg(feature = "soapy")]
    Soapy,
    #[cfg(feature = "hpsdr")]
    Hpsdr,
}

impl ReceiverKind {
    /// As the audio check names a receiver that does not use a sound card.
    pub fn label(self) -> &'static str {
        match self {
            ReceiverKind::SoundCard => "sound-card",
            ReceiverKind::File => "WAV file",
            ReceiverKind::Kiwi => "KiwiSDR",
            #[cfg(feature = "soapy")]
            ReceiverKind::Soapy => "SoapySDR",
            #[cfg(feature = "hpsdr")]
            ReceiverKind::Hpsdr => "HPSDR",
        }
    }

    fn fix(self) -> &'static str {
        match self {
            ReceiverKind::SoundCard => {
                "connect the radio's audio interface, or choose an input with --device or \
                 input.device (the audio check above lists this machine's inputs)"
            }
            ReceiverKind::File => {
                "check the path given with --source or input.path, and that this user can read it"
            }
            ReceiverKind::Kiwi => {
                "check --kiwi-host/input.host and --kiwi-port/input.port, and that the KiwiSDR \
                 has a free channel (open it in a web browser)"
            }
            #[cfg(feature = "soapy")]
            ReceiverKind::Soapy => {
                "check --soapy-driver/input.driver, and that SoapySDRUtil --find lists the \
                 radio; install its SoapySDR module if it does not"
            }
            #[cfg(feature = "hpsdr")]
            ReceiverKind::Hpsdr => {
                "check --hpsdr-host/input.host, and that the radio is powered on and on this \
                 network"
            }
        }
    }
}

/// The configured receiver, as doctor names it. Never holds a password.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReceiverDesc {
    pub kind: ReceiverKind,
    pub description: String,
}

/// The `receiver` line: whether the source opened, at what sample rate.
pub(crate) fn receiver_check(desc: &ReceiverDesc, result: Result<f64, &anyhow::Error>) -> Check {
    const NAME: &str = "receiver";
    match result {
        Ok(rate_hz) => Check::pass(
            NAME,
            format!("opened {} ({rate_hz:.0} Hz)", desc.description),
        ),
        Err(e) => Check::fail(
            NAME,
            format!("could not open {}: {e:#}", desc.description),
            desc.kind.fix(),
        ),
    }
}

/// The `signal` line, only when the decode run itself errored.
pub(crate) fn signal_check(e: &anyhow::Error) -> Check {
    Check::fail(
        "signal",
        format!("the receiver opened, but the signal check stopped: {e:#}"),
        "a WAV file must hold at least 3 seconds of audio; a live receiver must keep \
         delivering samples, so check its connection and run doctor again",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Env};
    use std::net::TcpListener;

    fn loaded(body: &str) -> (tempfile::TempDir, std::path::PathBuf, Loaded) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, body).unwrap();
        let l = config::load(Some(&path), Env::Ignore)
            .unwrap_or_else(|e| panic!("{body} did not load: {e:#}"));
        (dir, path, l)
    }

    fn server(lines: &str) -> ServerConfig {
        let (_dir, _path, l) = loaded(&format!("[server]\nstation_callsign = \"W1AW\"\n{lines}"));
        l.server.unwrap()
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn uplinks(body: &str) -> Vec<RbnUplinkConfig> {
        let (_dir, _path, l) = loaded(&format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\ntelnet_port = \
             0\njson_port = 0\nmetrics_port = 0\n{body}"
        ));
        l.rbn_uplink
    }

    fn uplink_block(host: &str, port: u16, enabled: bool) -> String {
        format!(
            "[[rbn_uplink]]\nenabled = {enabled}\ntarget_host = \"{host}\"\ntarget_port = \
             {port}\n"
        )
    }

    fn device(name: &str, inputs: u16) -> coppa_audio::AudioDevice {
        coppa_audio::AudioDevice {
            name: name.to_string(),
            max_sample_rate: 48_000,
            input_channels: inputs,
            output_channels: 2,
        }
    }

    // --- model and rendering -------------------------------------------

    #[test]
    fn render_puts_status_name_and_detail_on_one_line() {
        assert_eq!(
            render(&Check::pass("telnet port", "0.0.0.0:7300 is free")),
            "PASS  telnet port: 0.0.0.0:7300 is free\n"
        );
    }

    #[test]
    fn render_puts_fix_on_an_indented_second_line_for_warn_and_fail() {
        assert_eq!(
            render(&Check::fail("x", "d", "f")),
            "FAIL  x: d\n      fix: f\n"
        );
        assert_eq!(
            render(&Check::warn("x", "d", "f")),
            "WARN  x: d\n      fix: f\n"
        );
    }

    #[test]
    fn render_never_prints_a_fix_line_for_pass_or_skip() {
        assert_eq!(render(&Check::pass("x", "d")), "PASS  x: d\n");
        assert_eq!(render(&Check::skip("x", "d")), "SKIP  x: d\n");
        let mut odd = Check::skip("x", "d");
        odd.fix = Some("f".into());
        assert!(!render(&odd).contains("fix:"));
    }

    #[test]
    fn render_escapes_control_characters() {
        let out = render(&Check::fail("x\x1b[31m", "d\x1b[31m", "f\x1b[31m\n"));
        assert!(!out.contains('\x1b'), "{out}");
        assert_eq!(out.matches("\\u{1b}").count(), 3, "{out}");
        assert_eq!(out.lines().count(), 2, "{out}");
    }

    #[test]
    fn summary_when_everything_passed() {
        let mut checks = vec![Check::pass("a", "d"); 5];
        assert_eq!(summary(&checks), "doctor: all checks passed (5 passed)");
        checks.extend([Check::skip("s", "d"), Check::skip("t", "d")]);
        assert_eq!(
            summary(&checks),
            "doctor: all checks passed (5 passed, 2 skipped)"
        );
    }

    #[test]
    fn summary_names_warnings_only() {
        let mut checks = vec![Check::pass("a", "d"); 6];
        checks.insert(3, Check::warn("clock", "d", "f"));
        assert_eq!(summary(&checks), "doctor: 1 warning: clock (6 passed)");
        checks.push(Check::warn("other", "d", "f"));
        assert_eq!(
            summary(&checks),
            "doctor: 2 warnings: clock, other (6 passed)"
        );
    }

    #[test]
    fn summary_names_failures_then_warnings_in_check_order() {
        let checks = vec![
            Check::pass("config", "d"),
            Check::fail("telnet port", "d", "f"),
            Check::pass("json port", "d"),
            Check::skip("metrics port", "d"),
            Check::warn("clock", "d", "f"),
            Check::fail("rbn uplink a:1", "d", "f"),
            Check::pass("receiver", "d"),
            Check::pass("audio", "d"),
        ];
        assert_eq!(
            summary(&checks),
            "doctor: 2 failed: telnet port, rbn uplink a:1; 1 warning: clock (4 passed, 1 skipped)"
        );
    }

    #[test]
    fn checks_status_is_the_worst_status() {
        let pass = Check::pass("a", "d");
        let skip = Check::skip("b", "d");
        let warn = Check::warn("c", "d", "f");
        let fail = Check::fail("e", "d", "f");
        assert_eq!(worst(&[pass.clone(), skip.clone()]), Status::Pass);
        assert_eq!(worst(std::slice::from_ref(&skip)), Status::Pass);
        assert_eq!(worst(&[pass.clone(), warn.clone()]), Status::Warn);
        assert_eq!(worst(&[warn, fail, pass, skip]), Status::Fail);
    }

    #[test]
    fn check_serializes_with_lowercase_status_and_null_fix() {
        assert_eq!(
            serde_json::to_value(Check::pass("config", "ok")).unwrap(),
            serde_json::json!({"name": "config", "status": "pass", "detail": "ok", "fix": null})
        );
        assert_eq!(
            serde_json::to_value(Check::fail("x", "d", "f")).unwrap(),
            serde_json::json!({"name": "x", "status": "fail", "detail": "d", "fix": "f"})
        );
    }

    // --- config ---------------------------------------------------------

    #[test]
    fn config_check_skips_without_a_config_file() {
        let l = config::load(None, Env::Ignore).unwrap();
        assert_eq!(
            render(&config_check(None, &l, false)),
            "SKIP  config: no config file (--config or MANTA_CONFIG)\n"
        );
    }

    #[test]
    fn config_check_runs_on_a_server_table_from_the_environment_alone() {
        use std::ffi::OsString;
        let vars = [(
            OsString::from("MANTA_SERVER_STATION_CALLSIGN"),
            OsString::from("N0CALL"),
        )];
        let l = config::load(None, Env::Read(&vars)).unwrap();
        assert!(l.server.is_some());
        let check = config_check(None, &l, false);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check
                .detail
                .starts_with("server.station_callsign is still the example"),
            "{}",
            check.detail
        );
        assert!(check.fix.unwrap().contains("MANTA_* environment variables"));
        let vars = [(
            OsString::from("MANTA_SERVER_STATION_CALLSIGN"),
            OsString::from("W1AW"),
        )];
        let l = config::load(None, Env::Read(&vars)).unwrap();
        assert_eq!(config_check(None, &l, true).status, Status::Fail);
        assert_eq!(config_check(None, &l, false).status, Status::Pass);
    }

    #[test]
    fn config_check_fails_on_the_example_station_callsign() {
        let (_dir, path, l) = loaded("[server]\nstation_callsign = \"N0CALL\"\n");
        let check = config_check(Some(&path), &l, false);
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.detail,
            "server.station_callsign is still the example \"N0CALL\", so manta run refuses to start"
        );
        let fix = check.fix.unwrap();
        assert!(fix.contains("server.station_callsign"), "{fix}");
        assert!(fix.contains(&path.display().to_string()), "{fix}");
    }

    #[test]
    fn config_check_fails_on_the_example_uplink_login() {
        let (_dir, path, l) = loaded(
            "[server]\nstation_callsign = \"W1AW\"\n[[rbn_uplink]]\nenabled = true\n\
             target_host = \"h\"\ntarget_port = 1\nlogin_callsign = \"N0CALL\"\n",
        );
        let check = config_check(Some(&path), &l, false);
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.detail,
            "rbn_uplink.login_callsign is still the example \"N0CALL\", so manta run refuses to \
             start"
        );
        assert!(check.fix.unwrap().contains("rbn_uplink.login_callsign"));
    }

    #[test]
    fn config_check_fails_when_run_would_demand_a_dial_frequency() {
        let (_dir, path, l) = loaded(
            "[server]\nstation_callsign = \"W1AW\"\n[input]\ntype = \"file\"\npath = \"x.wav\"\n",
        );
        let resolved = crate::resolve(crate::CliOverrides::none(), &l).unwrap();
        let needs = crate::needs_dial_freq(
            l.server.is_some(),
            resolved.spec.is_rf_aware(),
            resolved.dial_freq_hz,
        );
        assert!(needs);
        let check = config_check(Some(&path), &l, needs);
        assert_eq!(check.status, Status::Fail);
        assert!(
            check
                .detail
                .contains("no dial frequency is set, so manta run refuses to start its servers"),
            "{}",
            check.detail
        );
        let fix = check.fix.unwrap();
        assert!(fix.contains("input.center_freq_hz") && fix.contains("--dial-freq-hz"));
    }

    #[test]
    fn config_check_passes_a_complete_config() {
        let (_dir, path, l) = loaded(
            "[server]\nstation_callsign = \"W1AW\"\n[input]\ntype = \"file\"\npath = \
             \"x.wav\"\ncenter_freq_hz = 7030000.0\n",
        );
        let resolved = crate::resolve(crate::CliOverrides::none(), &l).unwrap();
        let needs = crate::needs_dial_freq(
            l.server.is_some(),
            resolved.spec.is_rf_aware(),
            resolved.dial_freq_hz,
        );
        assert_eq!(
            render(&config_check(Some(&path), &l, needs)),
            format!(
                "PASS  config: manta run's start-up checks accept {}\n",
                path.display()
            )
        );
    }

    // --- ports ----------------------------------------------------------

    #[test]
    fn port_checks_are_one_skip_without_a_server_table() {
        let checks = port_checks(None);
        assert_eq!(checks.len(), 1);
        assert_eq!(
            render(&checks[0]),
            "SKIP  ports: no [server] table, so manta run starts no servers\n"
        );
    }

    #[test]
    fn port_zero_is_skipped() {
        let s =
            server("bind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n");
        let checks = port_checks(Some(&s));
        assert_eq!(checks.len(), 3);
        assert_eq!(
            render(&checks[0]),
            "SKIP  telnet port: port 0: the system picks a free port when manta run starts\n"
        );
        assert!(checks.iter().all(|c| c.status == Status::Skip));
    }

    #[test]
    fn a_free_port_passes() {
        let p = free_port();
        let s = server(&format!(
            "bind_addr = \"127.0.0.1\"\ntelnet_port = {p}\njson_port = 0\nmetrics_port = 0\n"
        ));
        let checks = port_checks(Some(&s));
        assert_eq!(
            render(&checks[0]),
            format!("PASS  telnet port: 127.0.0.1:{p} is free\n")
        );
    }

    #[test]
    fn a_held_port_fails_and_names_its_key() {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = held.local_addr().unwrap().port();
        let s = server(&format!(
            "bind_addr = \"127.0.0.1\"\ntelnet_port = 0\njson_port = {p}\nmetrics_port = 0\n"
        ));
        let check = &port_checks(Some(&s))[1];
        assert_eq!(check.name, "json port");
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.detail, format!("127.0.0.1:{p} is already in use"));
        let fix = check.fix.as_deref().unwrap();
        assert!(
            fix.contains("json_port") && fix.contains("manta status"),
            "{fix}"
        );
        drop(held);
    }

    #[test]
    fn a_duplicate_port_fails_without_binding() {
        let p = free_port();
        let s = server(&format!(
            "bind_addr = \"127.0.0.1\"\ntelnet_port = {p}\njson_port = {p}\nmetrics_port = 0\n"
        ));
        let checks = port_checks(Some(&s));
        assert_eq!(checks[0].status, Status::Pass, "{:?}", checks[0]);
        assert_eq!(checks[1].status, Status::Fail);
        assert_eq!(
            checks[1].detail,
            format!(
                "json_port {p} is the same as telnet_port, and two servers cannot share a port"
            )
        );
        assert_eq!(
            checks[1].fix.as_deref(),
            Some("set a different json_port in [server]")
        );
    }

    #[test]
    fn classify_bind_error_covers_each_kind() {
        let s = server(
            "bind_addr = \"0.0.0.0\"\nmetrics_bind_addr = \"127.0.0.1\"\ntelnet_port = \
             7300\njson_port = 7301\nmetrics_port = 7302\n",
        );
        let [telnet, _json, metrics] = config_cmd::server_listeners(&s);
        let err = |kind| io::Error::new(kind, "boom");
        let cases: &[(&Listener, io::ErrorKind, &str, &[&str])] = &[
            (
                &telnet,
                io::ErrorKind::AddrInUse,
                "0.0.0.0:7300 is already in use",
                &["port 7300", "telnet_port", "manta status"],
            ),
            (
                &telnet,
                io::ErrorKind::AddrNotAvailable,
                "0.0.0.0 is not an address of this machine",
                &["set bind_addr in [server]"],
            ),
            (
                &telnet,
                io::ErrorKind::PermissionDenied,
                "this user may not listen on port 7300",
                &["above 1023", "CAP_NET_BIND_SERVICE"],
            ),
            (
                &telnet,
                io::ErrorKind::Other,
                "could not listen on 0.0.0.0:7300: boom",
                &["check bind_addr and telnet_port in [server]"],
            ),
            (
                &metrics,
                io::ErrorKind::AddrNotAvailable,
                "127.0.0.1 is not an address of this machine",
                &["set metrics_bind_addr in [server]"],
            ),
            (
                &metrics,
                io::ErrorKind::Other,
                "could not listen on 127.0.0.1:7302: boom",
                &["check metrics_bind_addr and metrics_port in [server]"],
            ),
        ];
        for (listener, kind, detail, needles) in cases {
            let check = classify_bind_error(listener, &err(*kind));
            assert_eq!(check.status, Status::Fail);
            assert_eq!(check.detail, *detail);
            let fix = check.fix.unwrap();
            for needle in *needles {
                assert!(fix.contains(needle), "{kind}: {needle} not in {fix}");
            }
            if listener.name == "metrics" {
                assert!(!fix.contains(" bind_addr"), "{fix}");
            }
        }
    }

    // --- uplink ---------------------------------------------------------

    #[test]
    fn no_uplink_block_is_one_skip_row() {
        let checks = uplink_checks(&[], UPLINK_BUDGET, resolve_host, connect_host);
        assert_eq!(checks.len(), 1);
        assert_eq!(
            render(&checks[0]),
            "SKIP  rbn uplink: no [[rbn_uplink]] block to check\n"
        );
    }

    #[test]
    fn a_disabled_uplink_is_skipped_by_label() {
        let configs = uplinks(&uplink_block("h", 1, false));
        let checks = uplink_checks(&configs, UPLINK_BUDGET, resolve_host, connect_host);
        assert_eq!(
            render(&checks[0]),
            "SKIP  rbn uplink h:1: enabled = false, so manta run does not connect to it\n"
        );
    }

    #[test]
    fn a_listening_target_passes() {
        use std::io::Read as _;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let configs = uplinks(&uplink_block("127.0.0.1", port, true));
        let checks = uplink_checks(&configs, UPLINK_BUDGET, resolve_host, connect_host);
        assert_eq!(checks[0].name, format!("rbn uplink 127.0.0.1:{port}"));
        assert_eq!(checks[0].status, Status::Pass, "{:?}", checks[0]);
        assert!(
            checks[0].detail.starts_with("accepted a connection in ")
                && checks[0].detail.ends_with(" ms"),
            "{}",
            checks[0].detail
        );
        let (mut conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = Vec::new();
        assert_eq!(
            conn.read_to_end(&mut buf).unwrap(),
            0,
            "doctor sent {buf:?}"
        );
    }

    #[test]
    fn a_refused_target_fails_with_a_port_fix() {
        let port = free_port();
        let configs = uplinks(&uplink_block("127.0.0.1", port, true));
        let checks = uplink_checks(&configs, UPLINK_BUDGET, resolve_host, connect_host);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(
            checks[0].detail.starts_with("refused the connection ("),
            "{}",
            checks[0].detail
        );
        assert!(checks[0].fix.as_deref().unwrap().contains("target_port"));
    }

    #[test]
    fn an_unresolvable_target_fails_with_a_dns_fix() {
        let configs = uplinks(&uplink_block("no-such-host.invalid", 7000, true));
        let checks = uplink_checks(&configs, UPLINK_BUDGET, resolve_host, connect_host);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(
            checks[0]
                .detail
                .starts_with("could not look up no-such-host.invalid: "),
            "{}",
            checks[0].detail
        );
        let fix = checks[0].fix.as_deref().unwrap();
        assert!(
            fix.contains("target_host") && fix.contains("nslookup"),
            "{fix}"
        );
    }

    fn loopback(_: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
    }

    fn black_hole(_: &SocketAddr, _: Duration) -> io::Result<TcpStream> {
        std::thread::sleep(Duration::from_millis(300));
        Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
    }

    #[test]
    fn a_target_that_never_answers_fails_within_budget() {
        let started = Instant::now();
        let result = uplink_probe("h", 1, Duration::from_millis(200), loopback, black_hole);
        assert_eq!(result, Err(ProbeError::TimedOut));
        let configs = uplinks(&uplink_block("h", 1, true));
        let checks = uplink_checks(&configs, Duration::from_millis(200), loopback, black_hole);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(checks[0].status, Status::Fail);
        assert_eq!(checks[0].detail, "did not answer within 0.2 s");
        assert!(checks[0].fix.as_deref().unwrap().contains("firewall"));
    }

    #[test]
    fn duplicate_targets_get_distinct_labels() {
        let configs = uplinks(&format!(
            "{}{}",
            uplink_block("h", 1, false),
            uplink_block("h", 1, false)
        ));
        let checks = uplink_checks(&configs, UPLINK_BUDGET, resolve_host, connect_host);
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["rbn uplink h:1", "rbn uplink h:1#2"]);
    }

    #[test]
    fn targets_are_probed_concurrently() {
        let configs = uplinks(&format!(
            "{}{}",
            uplink_block("a", 1, true),
            uplink_block("b", 2, true)
        ));
        let started = Instant::now();
        let checks = uplink_checks(&configs, Duration::from_secs(5), loopback, black_hole);
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(checks.len(), 2);
        assert!(checks.iter().all(|c| c.status == Status::Fail));
    }

    // --- audio ----------------------------------------------------------

    fn lib() -> &'static str {
        if cfg!(target_os = "linux") {
            "ALSA runtime library is installed; "
        } else {
            ""
        }
    }

    #[test]
    fn audio_is_not_enumerated_for_a_non_sound_card_receiver() {
        let check = audio_check("KiwiSDR", false, || panic!("must not enumerate"));
        assert_eq!(check.status, Status::Pass);
        assert_eq!(
            check.detail,
            format!("{}this KiwiSDR receiver does not use a sound card", lib())
        );
    }

    #[test]
    fn audio_lists_input_names_and_ignores_output_only_devices() {
        let check = audio_check("sound-card", true, || {
            Ok(vec![device("Speakers", 0), device("USB Audio CODEC", 2)])
        });
        assert_eq!(check.status, Status::Pass);
        assert_eq!(
            check.detail,
            format!("{}1 sound-card input: \"USB Audio CODEC\"", lib())
        );
    }

    #[test]
    fn audio_truncates_after_four_names() {
        let check = audio_check("sound-card", true, || {
            Ok(["A", "B", "C", "D", "E", "F"]
                .iter()
                .map(|n| device(n, 1))
                .collect())
        });
        assert_eq!(
            check.detail,
            format!(
                "{}6 sound-card inputs: \"A\", \"B\", \"C\", \"D\", and 2 more",
                lib()
            )
        );
    }

    #[test]
    fn audio_fails_with_no_inputs() {
        let check = audio_check("sound-card", true, || Ok(vec![device("Speakers", 0)]));
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.detail, "the audio system lists no sound-card inputs");
        assert!(check
            .fix
            .unwrap()
            .contains("connect the radio's audio interface"));
    }

    #[test]
    fn audio_fails_when_enumeration_errors() {
        let check = audio_check("sound-card", true, || Err(anyhow::anyhow!("no backend")));
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.detail,
            "the audio system could not list inputs: no backend"
        );
        assert!(check.fix.is_some());
    }

    #[test]
    fn audio_detail_names_alsa_only_on_linux() {
        let check = audio_check("KiwiSDR", false, || Ok(vec![]));
        assert_eq!(
            check.detail.contains("ALSA"),
            cfg!(target_os = "linux"),
            "{}",
            check.detail
        );
        let failed = audio_check("sound-card", true, || Ok(vec![]));
        assert_eq!(
            failed.fix.unwrap().contains("arecord -l"),
            cfg!(target_os = "linux")
        );
    }

    // --- receiver and signal -------------------------------------------

    fn desc(spec: crate::LiveSourceSpec) -> ReceiverDesc {
        crate::receiver_desc(&spec)
    }

    fn kiwi(password: &str) -> crate::LiveSourceSpec {
        crate::LiveSourceSpec::Kiwi(crate::KiwiOpts {
            host: Some("kiwi.example.net".into()),
            port: 8073,
            freq: Some(7_030_000.0),
            password: password.into(),
        })
    }

    #[test]
    fn receiver_pass_names_the_source_and_rate() {
        let file = desc(crate::LiveSourceSpec::File {
            path: "/x.wav".into(),
            source_iq: false,
        });
        assert_eq!(
            render(&receiver_check(&file, Ok(48_000.0))),
            "PASS  receiver: opened the WAV file /x.wav (48000 Hz)\n"
        );
    }

    #[test]
    fn receiver_fail_has_a_per_kind_fix() {
        let err = anyhow::anyhow!("inner").context("outer");
        #[cfg_attr(not(any(feature = "soapy", feature = "hpsdr")), allow(unused_mut))]
        let mut cases: Vec<(crate::LiveSourceSpec, &str, &[&str])> = vec![
            (
                crate::LiveSourceSpec::AudioDevice(None),
                "could not open the system default sound-card input: outer: inner",
                &["--device", "input.device"],
            ),
            (
                crate::LiveSourceSpec::AudioDevice(Some("USB".into())),
                "could not open the sound-card input matching \"USB\": outer: inner",
                &["--device", "input.device"],
            ),
            (
                crate::LiveSourceSpec::File {
                    path: "/x.wav".into(),
                    source_iq: true,
                },
                "could not open the WAV file /x.wav: outer: inner",
                &["--source", "input.path"],
            ),
            (
                kiwi(""),
                "could not open KiwiSDR kiwi.example.net:8073: outer: inner",
                &["--kiwi-host", "--kiwi-port", "free channel"],
            ),
        ];
        #[cfg(feature = "soapy")]
        cases.push((
            crate::LiveSourceSpec::Soapy(crate::SoapyOpts {
                driver: Some("driver=rtlsdr".into()),
                freq: Some(7e6),
                rate: Some(48_000.0),
                gain: None,
            }),
            "could not open SoapySDR device \"driver=rtlsdr\": outer: inner",
            &["--soapy-driver", "SoapySDRUtil --find"],
        ));
        #[cfg(feature = "hpsdr")]
        cases.push((
            crate::LiveSourceSpec::Hpsdr(crate::HpsdrOpts {
                host: Some("10.0.0.5".into()),
                port: 1024,
                freq: Some(7e6),
                rate: Some(48_000.0),
            }),
            "could not open HPSDR radio 10.0.0.5:1024: outer: inner",
            &["--hpsdr-host", "powered on"],
        ));
        for (spec, detail, needles) in cases {
            let check = receiver_check(&desc(spec), Err(&err));
            assert_eq!(check.status, Status::Fail);
            assert_eq!(check.detail, detail);
            let fix = check.fix.unwrap();
            for needle in needles {
                assert!(fix.contains(needle), "{needle} not in {fix}");
            }
        }
    }

    #[test]
    fn receiver_never_prints_the_kiwi_password() {
        let d = desc(kiwi("hunter2"));
        let err = anyhow::anyhow!("refused");
        for check in [
            receiver_check(&d, Ok(12_000.0)),
            receiver_check(&d, Err(&err)),
        ] {
            let text = render(&check);
            assert!(!text.contains("hunter2"), "{text}");
            let json = serde_json::to_string(&check).unwrap();
            assert!(!json.contains("hunter2"), "{json}");
        }
    }

    #[test]
    fn signal_failure_is_a_named_fail() {
        let check = signal_check(&anyhow::anyhow!("stream ended").context("decode"));
        assert_eq!(check.name, "signal");
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.detail,
            "the receiver opened, but the signal check stopped: decode: stream ended"
        );
        assert!(check.fix.unwrap().contains("at least 3 seconds"));
    }

    // --- docs -------------------------------------------------------------

    fn repo_file(rel: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn runbook_documents_every_check_and_status() {
        let runbook = repo_file("docs/RUNBOOKS/setup-checks.md");
        for needle in [
            "`config`",
            "`audio`",
            "`telnet port`",
            "`json port`",
            "`metrics port`",
            "`ports`",
            "`clock`",
            "`rbn uplink",
            "`receiver`",
            "`signal`",
            "PASS",
            "WARN",
            "FAIL",
            "SKIP",
            "--ntp-server",
            "| 0 |",
            "| 1 |",
        ] {
            assert!(runbook.contains(needle), "setup-checks.md lacks {needle}");
        }
    }

    #[test]
    fn runbook_and_packaging_readme_name_the_loader_error() {
        for rel in ["docs/RUNBOOKS/setup-checks.md", "packaging/README.md"] {
            let text = repo_file(rel);
            for needle in ["libasound.so.2", "error while loading shared libraries"] {
                assert!(text.contains(needle), "{rel} lacks {needle}");
            }
        }
    }
}
