//! `manta doctor`'s `clock` check: one SNTP v4 request for the offset that
//! matters (spot times), plus, on Linux, the kernel's own NTP-sync state.
//! See docs/DECISIONS/2026-10-10-man126-doctor-setup-checks.md (D5).

use crate::doctor_checks::Check;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const DEFAULT_NTP_SERVER: &str = "pool.ntp.org";
pub(crate) const NTP_BUDGET: Duration = Duration::from_secs(3);
/// The shortest wait for one of a server's addresses: a satellite link's
/// round trip, with room to spare.
const NTP_ATTEMPT_MIN: Duration = Duration::from_secs(1);
/// Far outside the error any working NTP client allows.
pub(crate) const WARN_OFFSET_S: f64 = 1.0;
/// The RBN spot line's one-minute time resolution: past it, every spot
/// carries the wrong minute.
pub(crate) const FAIL_OFFSET_S: f64 = 60.0;

const NTP_PORT: u16 = 123;
const PACKET_LEN: usize = 48;
/// Seconds from the NTP era-0 epoch (1900-01-01) to the Unix epoch.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// `--ntp-server`: HOST, HOST:PORT, `[IPv6]:PORT` or a bare IPv6 address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NtpServer {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for NtpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.port == NTP_PORT {
            write!(f, "{}", self.host)
        } else if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// clap's `value_parser` for `--ntp-server`.
pub(crate) fn parse_ntp_server(s: &str) -> Result<NtpServer, String> {
    let bad = || format!("expected HOST or HOST:PORT, got \"{s}\"");
    let port_of = |p: &str| p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad);
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(bad)?;
        let port = match after {
            "" => NTP_PORT,
            _ => port_of(after.strip_prefix(':').ok_or_else(bad)?)?,
        };
        (host, port)
    } else if s.matches(':').count() >= 2 {
        s.parse::<Ipv6Addr>().map_err(|_| bad())?;
        (s, NTP_PORT)
    } else if let Some((host, port)) = s.split_once(':') {
        (host, port_of(port)?)
    } else {
        (s, NTP_PORT)
    };
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad());
    }
    Ok(NtpServer {
        host: host.to_string(),
        port,
    })
}

/// A Unix time as an NTP timestamp: era-0 seconds and a 32-bit fraction.
fn to_ntp(t: SystemTime) -> [u8; 8] {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    // Truncation to 32 bits is the NTP era wrap; `from_ntp` undoes it.
    let secs = (d.as_secs() + NTP_UNIX_OFFSET) as u32;
    let frac = ((u64::from(d.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&secs.to_be_bytes());
    out[4..].copy_from_slice(&frac.to_be_bytes());
    out
}

/// An NTP timestamp as Unix seconds. RFC 4330 §3: a clear top bit means
/// era 1 (2036-02-07 onwards).
fn from_ntp(b: &[u8]) -> f64 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let secs = if secs & 0x8000_0000 == 0 {
        u64::from(secs) + (1u64 << 32)
    } else {
        u64::from(secs)
    };
    secs as f64 - NTP_UNIX_OFFSET as f64 + f64::from(frac) / 4_294_967_296.0
}

fn unix_seconds(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// A v4 client request (LI 0, VN 4, mode 3) carrying `transmit`.
fn request(transmit: [u8; 8]) -> [u8; PACKET_LEN] {
    let mut packet = [0u8; PACKET_LEN];
    packet[0] = 0x23;
    packet[40..48].copy_from_slice(&transmit);
    packet
}

/// RFC 4330's clock offset: positive when the server is ahead of this clock.
fn offset(t0: f64, t1: f64, t2: f64, t3: f64) -> f64 {
    ((t1 - t0) + (t2 - t3)) / 2.0
}

/// Checks a server reply against the request that carried `sent`, and
/// returns the offset. `t3` is the Unix time the reply arrived. Errors are
/// phrased to follow the server's name.
fn validate(reply: &[u8], sent: &[u8; 8], t3: f64) -> Result<f64, String> {
    if reply.len() < PACKET_LEN {
        return Err("sent a short reply".to_string());
    }
    if reply[0] >> 6 == 3 {
        return Err("reports that its own clock is not synchronized".to_string());
    }
    if reply[0] & 0x07 != 4 {
        return Err("did not reply as an NTP server".to_string());
    }
    if reply[1] == 0 {
        return Err("refused the request (a kiss-of-death reply)".to_string());
    }
    if reply[1] > 15 {
        return Err(format!("sent a reserved stratum ({})", reply[1]));
    }
    if reply[24..32] != sent[..] {
        return Err("sent a reply that does not match the request".to_string());
    }
    Ok(offset(
        from_ntp(sent),
        from_ntp(&reply[32..40]),
        from_ntp(&reply[40..48]),
        t3,
    ))
}

/// `3 s`, or `0.3 s` for a budget with a fraction.
pub(crate) fn fmt_budget(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// One SNTP exchange with `server`, trying each of its addresses within
/// the budget. The offset is positive when this clock is behind.
pub(crate) fn query(server: &NtpServer, budget: Duration) -> Result<f64, String> {
    let deadline = Instant::now() + budget;
    let addrs: Vec<SocketAddr> = (server.host.as_str(), server.port)
        .to_socket_addrs()
        .map_err(|e| format!("could not be looked up ({e})"))?
        .collect();
    if addrs.is_empty() {
        return Err("could not be looked up (no addresses)".to_string());
    }
    query_addrs(&addrs, budget, deadline)
}

/// Tries `addrs` in order until one answers or `deadline` passes. Each
/// address gets an even share of what is left, but at least
/// `NTP_ATTEMPT_MIN`: a silent address cannot spend the time the ones after
/// it need, and a slow link still has time to answer.
fn query_addrs(addrs: &[SocketAddr], budget: Duration, deadline: Instant) -> Result<f64, String> {
    let timed_out = || format!("did not answer within {}", fmt_budget(budget));
    let mut last = None;
    for (i, addr) in addrs.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let left = u32::try_from(addrs.len() - i).unwrap_or(u32::MAX);
        let attempt = (remaining / left).max(NTP_ATTEMPT_MIN).min(remaining);
        match query_addr(*addr, attempt) {
            Ok(offset) => return Ok(offset),
            Err(Some(e)) => last = Some(e),
            Err(None) => last = Some(timed_out()),
        }
    }
    Err(last.unwrap_or_else(timed_out))
}

/// `Err(None)` is a timeout, worded by the caller.
fn query_addr(addr: SocketAddr, timeout: Duration) -> Result<f64, Option<String>> {
    let local: SocketAddr = if addr.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let io = |e: std::io::Error| Some(format!("could not be reached ({e})"));
    let sock = UdpSocket::bind(local).map_err(io)?;
    // Connected, so only this server's replies are read, and an ICMP
    // refusal surfaces at once instead of after the whole budget.
    sock.connect(addr).map_err(io)?;
    sock.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
        .map_err(io)?;
    let sent = to_ntp(SystemTime::now());
    sock.send(&request(sent)).map_err(io)?;
    let mut buf = [0u8; 64];
    let n = match sock.recv(&mut buf) {
        Ok(n) => n,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err(None)
        }
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return Err(Some(format!(
                "refused the request (nothing answers on UDP port {})",
                addr.port()
            )))
        }
        Err(e) => return Err(io(e)),
    };
    let t3 = unix_seconds(SystemTime::now());
    validate(&buf[..n], &sent, t3).map_err(Some)
}

/// The kernel's NTP state: `Some(true)` when a time daemon (chrony,
/// systemd-timesyncd, ntpd) is disciplining the clock. `adjtimex` with
/// `modes = 0` only reads, and needs no privilege. `None` off Linux.
#[cfg(target_os = "linux")]
pub(crate) fn kernel_synced() -> Option<bool> {
    // SAFETY: `timex` is a plain C struct for which all-zero bytes are a
    // valid value, and `modes = 0` makes `adjtimex` a read-only query that
    // only writes into the struct we own.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut tx) };
    if state < 0 {
        return None;
    }
    Some(state != libc::TIME_ERROR && tx.status & libc::STA_UNSYNC == 0)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn kernel_synced() -> Option<bool> {
    None
}

/// How an operator turns network time on, on this platform.
pub(crate) fn platform_clock_fix() -> &'static str {
    if cfg!(target_os = "linux") {
        "turn on network time: `sudo timedatectl set-ntp true`, or install and start chrony"
    } else if cfg!(target_os = "macos") {
        "turn on System Settings > General > Date & Time > Set time and date automatically"
    } else if cfg!(windows) {
        "turn on Settings > Time & language > Date & time > Set the time automatically"
    } else {
        "turn on this machine's network time synchronization"
    }
}

/// Seconds as doctor prints them: 3 decimals under 1 s, else 1.
fn fmt_offset(s: f64) -> String {
    if s < WARN_OFFSET_S {
        format!("{s:.3}")
    } else {
        format!("{s:.1}")
    }
}

/// The `clock` line from the SNTP result and the kernel's sync state.
pub(crate) fn classify(server: &str, offset: Result<f64, String>, kernel: Option<bool>) -> Check {
    const NAME: &str = "clock";
    let ntp_fix = format!(
        "allow outbound UDP port 123 to {server}, or name an NTP server this machine can reach \
         with --ntp-server"
    );
    match offset {
        Ok(o) => {
            let size = o.abs();
            let way = if o > 0.0 { "behind" } else { "ahead of" };
            let s = fmt_offset(size);
            if size >= FAIL_OFFSET_S {
                Check::fail(
                    NAME,
                    format!("this clock is {s} s {way} {server}, so every spot's time is wrong"),
                    platform_clock_fix(),
                )
            } else if size >= WARN_OFFSET_S {
                Check::warn(
                    NAME,
                    format!(
                        "this clock is {s} s {way} {server}, so spot times are off by that much"
                    ),
                    platform_clock_fix(),
                )
            } else {
                match kernel {
                    Some(true) => Check::pass(
                        NAME,
                        format!("within {s} s of {server}, and NTP is keeping it in sync"),
                    ),
                    None => Check::pass(NAME, format!("within {s} s of {server}")),
                    Some(false) => Check::warn(
                        NAME,
                        format!(
                            "within {s} s of {server} now, but nothing is keeping it in sync, \
                             so it will drift"
                        ),
                        platform_clock_fix(),
                    ),
                }
            }
        }
        Err(e) => match kernel {
            Some(true) => Check::pass(
                NAME,
                format!("{server} did not answer, but NTP is keeping this clock in sync"),
            ),
            Some(false) => Check::warn(
                NAME,
                format!(
                    "could not measure the clock: {server} {e}, and the kernel reports it is \
                     not NTP-synchronized"
                ),
                format!("{ntp_fix}; {}", platform_clock_fix()),
            ),
            None => Check::warn(
                NAME,
                format!("could not measure the clock: {server} {e}"),
                ntp_fix,
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor_checks::Status;

    fn reply(li: u8, mode: u8, stratum: u8, originate: [u8; 8], t1: f64, t2: f64) -> Vec<u8> {
        let mut p = vec![0u8; PACKET_LEN];
        p[0] = (li << 6) | (4 << 3) | mode;
        p[1] = stratum;
        p[24..32].copy_from_slice(&originate);
        let at = |s: f64| to_ntp(UNIX_EPOCH + Duration::from_secs_f64(s));
        p[32..40].copy_from_slice(&at(t1));
        p[40..48].copy_from_slice(&at(t2));
        p
    }

    fn sent_at(s: f64) -> [u8; 8] {
        to_ntp(UNIX_EPOCH + Duration::from_secs_f64(s))
    }

    #[test]
    fn ntp_timestamps_round_trip() {
        for s in [0.0, 1_000_000_000.25, 1_791_000_000.5, 2_200_000_000.75] {
            let back = from_ntp(&to_ntp(UNIX_EPOCH + Duration::from_secs_f64(s)));
            assert!((back - s).abs() < 1e-6, "{s} -> {back}");
        }
    }

    #[test]
    fn request_is_a_v4_client_packet_carrying_the_send_time() {
        let t0 = UNIX_EPOCH + Duration::from_millis(1_791_000_000_500);
        let p = request(to_ntp(t0));
        assert_eq!(p.len(), 48);
        assert_eq!(p[0], 0x23);
        assert_eq!(
            u32::from_be_bytes([p[40], p[41], p[42], p[43]]) as u64,
            1_791_000_000 + NTP_UNIX_OFFSET
        );
        assert_eq!(u32::from_be_bytes([p[44], p[45], p[46], p[47]]), 1 << 31);
        assert!(p[1..40].iter().all(|b| *b == 0));
    }

    #[test]
    fn offset_uses_the_four_timestamp_formula() {
        assert!((offset(100.0, 130.0, 130.0, 100.2) - 29.9).abs() < 1e-9);
        let sent = sent_at(100.0);
        let got = validate(&reply(0, 4, 2, sent, 130.0, 130.0), &sent, 100.2).unwrap();
        assert!((got - 29.9).abs() < 1e-6, "{got}");
    }

    #[test]
    fn reply_is_rejected_when_the_originate_does_not_echo_our_send_time() {
        let sent = sent_at(100.0);
        let err = validate(&reply(0, 4, 2, sent_at(99.0), 130.0, 130.0), &sent, 100.2);
        assert!(err.unwrap_err().contains("does not match"));
    }

    #[test]
    fn reply_is_rejected_for_kiss_of_death_stratum_0() {
        let sent = sent_at(100.0);
        let err = validate(&reply(0, 4, 0, sent, 130.0, 130.0), &sent, 100.2);
        assert!(err.unwrap_err().contains("kiss-of-death"));
    }

    #[test]
    fn reply_strata_are_limited_to_synchronized_sources() {
        let sent = sent_at(100.0);
        for stratum in 1..=15 {
            assert!(
                validate(&reply(0, 4, stratum, sent, 100.0, 100.0), &sent, 100.0).is_ok(),
                "stratum {stratum}"
            );
        }
        for stratum in 16..=255 {
            assert!(
                validate(&reply(0, 4, stratum, sent, 100.0, 100.0), &sent, 100.0).is_err(),
                "reserved stratum {stratum} was accepted"
            );
        }
    }

    #[test]
    fn reply_is_rejected_for_leap_indicator_3() {
        let sent = sent_at(100.0);
        let err = validate(&reply(3, 4, 2, sent, 130.0, 130.0), &sent, 100.2);
        assert!(err.unwrap_err().contains("not synchronized"));
    }

    #[test]
    fn reply_is_rejected_unless_mode_is_server() {
        let sent = sent_at(100.0);
        for mode in [0, 1, 2, 3, 5, 6, 7] {
            let err = validate(&reply(0, mode, 2, sent, 130.0, 130.0), &sent, 100.2);
            assert!(err.is_err(), "mode {mode}");
        }
    }

    #[test]
    fn reply_is_rejected_when_shorter_than_48_bytes() {
        let sent = sent_at(100.0);
        let full = reply(0, 4, 2, sent, 130.0, 130.0);
        assert!(validate(&full[..47], &sent, 100.2)
            .unwrap_err()
            .contains("short"));
    }

    #[test]
    fn parse_ntp_server_accepts_host_host_port_and_bracketed_ipv6() {
        let pool = parse_ntp_server("pool.ntp.org").unwrap();
        assert_eq!((pool.host.as_str(), pool.port), ("pool.ntp.org", 123));
        assert_eq!(pool.to_string(), "pool.ntp.org");
        let local = parse_ntp_server("127.0.0.1:1234").unwrap();
        assert_eq!((local.host.as_str(), local.port), ("127.0.0.1", 1234));
        assert_eq!(local.to_string(), "127.0.0.1:1234");
        let v6 = parse_ntp_server("[::1]:123").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 123));
        let v6_port = parse_ntp_server("[::1]:1234").unwrap();
        assert_eq!(v6_port.to_string(), "[::1]:1234");
        let bare = parse_ntp_server("::1").unwrap();
        assert_eq!((bare.host.as_str(), bare.port), ("::1", 123));
        for bad in ["", "host:", "host:abc", "host:0", "[::1", "[::1]x", "a b"] {
            let err = parse_ntp_server(bad).unwrap_err();
            assert!(err.contains("expected HOST or HOST:PORT"), "{bad}: {err}");
        }
    }

    /// SNTP result, kernel sync state, expected status and detail.
    type Case = (Result<f64, String>, Option<bool>, Status, &'static str);

    #[test]
    fn classify_covers_each_offset_and_kernel_state() {
        let fix = platform_clock_fix();
        let cases: &[Case] = &[
            (
                Ok(0.004),
                Some(true),
                Status::Pass,
                "within 0.004 s of S, and NTP is keeping it in sync",
            ),
            (Ok(0.004), None, Status::Pass, "within 0.004 s of S"),
            (
                Ok(0.004),
                Some(false),
                Status::Warn,
                "within 0.004 s of S now, but nothing is keeping it in sync, so it will drift",
            ),
            (
                Ok(2.4),
                Some(true),
                Status::Warn,
                "this clock is 2.4 s behind S, so spot times are off by that much",
            ),
            (
                Ok(-2.4),
                None,
                Status::Warn,
                "this clock is 2.4 s ahead of S, so spot times are off by that much",
            ),
            (
                Ok(75.0),
                Some(true),
                Status::Fail,
                "this clock is 75.0 s behind S, so every spot's time is wrong",
            ),
            (
                Err("did not answer within 3 s".into()),
                Some(true),
                Status::Pass,
                "S did not answer, but NTP is keeping this clock in sync",
            ),
            (
                Err("did not answer within 3 s".into()),
                Some(false),
                Status::Warn,
                "could not measure the clock: S did not answer within 3 s, and the kernel \
                 reports it is not NTP-synchronized",
            ),
            (
                Err("did not answer within 3 s".into()),
                None,
                Status::Warn,
                "could not measure the clock: S did not answer within 3 s",
            ),
            (Ok(0.9994), None, Status::Pass, "within 0.999 s of S"),
            (
                Ok(1.0),
                None,
                Status::Warn,
                "this clock is 1.0 s behind S, so spot times are off by that much",
            ),
            (
                Ok(60.0),
                None,
                Status::Fail,
                "this clock is 60.0 s behind S, so every spot's time is wrong",
            ),
        ];
        for (offset, kernel, status, detail) in cases {
            let check = classify("S", offset.clone(), *kernel);
            assert_eq!(check.name, "clock");
            assert_eq!((check.status, check.detail.as_str()), (*status, *detail));
            match check.status {
                Status::Pass => assert_eq!(check.fix, None),
                _ => assert!(check.fix.is_some(), "{detail}"),
            }
            if offset.is_ok() && check.status != Status::Pass {
                assert_eq!(check.fix.as_deref(), Some(fix), "{detail}");
            }
        }
        let silent = classify("S", Err("did not answer within 3 s".into()), None);
        let silent_fix = silent.fix.unwrap();
        assert!(silent_fix.contains("--ntp-server") && silent_fix.contains("UDP port 123"));
        let both = classify("S", Err("x".into()), Some(false)).fix.unwrap();
        assert!(
            both.contains("--ntp-server") && both.contains(fix),
            "{both}"
        );
    }

    /// A fake SNTP server on loopback whose clock runs `ahead_s` fast.
    fn fake_server(ahead_s: f64) -> (UdpSocket, NtpServer) {
        fake_server_after(ahead_s, Duration::ZERO)
    }

    /// As `fake_server`, but each reply leaves `delay` after the request
    /// arrives, like a server on a slow link.
    fn fake_server_after(ahead_s: f64, delay: Duration) -> (UdpSocket, NtpServer) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let answer = sock.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = answer.recv_from(&mut buf) {
                if n < PACKET_LEN {
                    continue;
                }
                let t1 = unix_seconds(SystemTime::now()) + ahead_s;
                std::thread::sleep(delay);
                let t2 = unix_seconds(SystemTime::now()) + ahead_s;
                let mut originate = [0u8; 8];
                originate.copy_from_slice(&buf[40..48]);
                let p = reply(0, 4, 1, originate, t1, t2);
                let _ = answer.send_to(&p, from);
            }
        });
        (
            sock,
            NtpServer {
                host: "127.0.0.1".into(),
                port,
            },
        )
    }

    #[test]
    fn query_measures_a_local_fake_servers_offset() {
        let (_sock, server) = fake_server(30.0);
        let got = query(&server, Duration::from_secs(3)).unwrap();
        assert!((29.5..=30.5).contains(&got), "{got}");
    }

    #[test]
    fn query_reports_a_silent_server_within_its_budget() {
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = NtpServer {
            host: "127.0.0.1".into(),
            port: silent.local_addr().unwrap().port(),
        };
        let started = Instant::now();
        let err = query(&server, Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1), "{err}");
        assert_eq!(err, "did not answer within 0.3 s");
    }

    #[test]
    fn query_tries_a_later_address_after_a_silent_first_one() {
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (_sock, server) = fake_server(30.0);
        let answering: SocketAddr = (Ipv4Addr::LOCALHOST, server.port).into();
        let addrs = [silent.local_addr().unwrap(), answering];
        let budget = Duration::from_secs(2);
        let got = query_addrs(&addrs, budget, Instant::now() + budget).unwrap();
        assert!((29.5..=30.5).contains(&got), "{got}");
    }

    #[test]
    fn query_gives_a_slow_first_address_time_to_answer() {
        let (_sock, server) = fake_server_after(30.0, Duration::from_millis(600));
        let slow: SocketAddr = (Ipv4Addr::LOCALHOST, server.port).into();
        let budget = Duration::from_secs(2);
        let got = query_addrs(&[slow; 4], budget, Instant::now() + budget).unwrap();
        assert!((29.5..=30.5).contains(&got), "{got}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_state_is_readable_without_privilege() {
        assert!(kernel_synced().is_some());
    }
}
