//! Outbound RBN telnet uplink -- MAN-32. Connects to RBN's own
//! spot-collection endpoint as a client and forwards manta's own
//! validated spots (`SpotBus`) in the same `DX de` wire format the
//! inbound telnet server (`telnet.rs`) emits, per ARCHITECTURE §7's wire
//! format and this repo's own reference implementation of that protocol
//! from the server side.

use crate::backoff::{next_backoff, AttemptOutcome as ConnectAttemptError, INITIAL_BACKOFF};
use crate::bounded_io::{read_line_bounded, read_line_bounded_with_timeout};
use crate::bus::SpotBus;
use crate::config::{RbnUplinkConfig, UplinkSpotTypes};
use crate::metrics::UplinkTarget;
use crate::rate_limit::RateLimiter;
use crate::rbn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch, Semaphore};

/// Bounds `TcpStream::connect` (MAN-58 comment finding 1): a target that
/// silently black-holes SYNs (e.g. a firewall drop, not a refusal) would
/// otherwise leave this attempt pending for the OS's own connect timeout
/// (commonly minutes), blocking shutdown the whole time -- this attempt
/// is additionally raced against `shutdown.changed()` below, but the
/// timeout still bounds worst-case time when shutdown never fires.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds the WHOLE multi-address attempt in `connect_first_reachable`
/// (MAN-67, PR #80 review round 9): each candidate address already gets
/// its own `CONNECT_TIMEOUT`, but with no cap on the loop as a whole, a
/// hostname resolving to a long list of black-holed addresses could defer
/// the next resolution/retry by `address_count * CONNECT_TIMEOUT` --
/// unbounded in practice, since a bad DNS response can list arbitrarily
/// many candidates. 3x `CONNECT_TIMEOUT`: enough to fall through to a
/// second or third address (the actual case this uplink needs to
/// tolerate) without still granting an effectively unbounded budget to a
/// long candidate list. Trades off the same way PR #80 round 2's
/// per-address timeout already does: a later address that would have
/// succeeded can still be cut off if earlier ones ate the whole window,
/// but the alternative (no cap) is worse -- shutdown remains the only
/// hard bound otherwise.
const OVERALL_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Every outbound write to the uplink target gets this long before being
/// treated as stalled (MAN-58 comment finding 2) -- matches `telnet.rs`'s
/// identical `WRITE_TIMEOUT` for the same class of risk on the inbound
/// side: a target that completes login but stops reading (TCP receive
/// window fills, then the local send buffer fills) must not block this
/// task indefinitely.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Response-line rate budget for the target's post-login discard read
/// (MAN-58 comment finding 3): RBN's collection server isn't expected to
/// send anything meaningful back after login, but with no budget at all
/// a misbehaving, compromised, or MITM'd target sending an endless stream
/// of short, valid, newline-terminated lines would keep this task hot
/// indefinitely with no CPU/bandwidth bound -- mirrors `telnet.rs`'s
/// `MAX_TELNET_COMMANDS`/`COMMAND_RATE_WINDOW` budget for the same class
/// of risk on the inbound side. A RATE, not a lifetime total: a
/// long-running connection where the target occasionally sends a stray
/// line must never be disconnected just for staying connected a long
/// time.
const MAX_TARGET_RESPONSE_LINES: u32 = 30;
const TARGET_RESPONSE_RATE_WINDOW: Duration = Duration::from_secs(60);

/// MAN-128 D7: the Prometheus label for each configured `[[rbn_uplink]]`
/// target is `host:port`; the 2nd and later exact duplicate of the same
/// `host:port` gets `#N` appended (`N` = occurrence number), so two targets
/// that happen to share a host:port still render as distinct series rather
/// than colliding into one. Matches PR #95 (MAN-44)'s convention so the two
/// converge on one registry shape.
pub fn target_labels(configs: &[RbnUplinkConfig]) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    configs
        .iter()
        .map(|c| {
            let base = format!("{}:{}", c.target_host, c.target_port);
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                base
            } else {
                format!("{base}#{count}")
            }
        })
        .collect()
}

/// Reconnect-with-backoff loop around one uplink connection. Never
/// returns while `config.enabled` and the shutdown signal hasn't fired --
/// a dropped connection must not permanently silence the uplink, since
/// that would defeat MAN-32's purpose of manta staying a live RBN
/// contributor.
///
/// `dry_run` is fixed at `config.dry_run` for the task's life; see
/// `serve_with_live_dry_run` for a flag a live reload can change.
pub async fn serve(
    config: RbnUplinkConfig,
    station_callsign: String,
    bus: Arc<SpotBus>,
    target: Arc<UplinkTarget>,
    shutdown: watch::Receiver<bool>,
) {
    let dry_run = Arc::new(AtomicBool::new(config.dry_run));
    serve_with_live_dry_run(config, station_callsign, bus, target, dry_run, shutdown).await;
}

/// `serve`, reading `dry_run` from a shared flag on every forwarded spot
/// instead of `config.dry_run` (MAN-78): a live config reload flips it
/// without reconnecting or logging in again. Once running, only the flag
/// is read, never `config.dry_run`.
pub async fn serve_with_live_dry_run(
    config: RbnUplinkConfig,
    station_callsign: String,
    bus: Arc<SpotBus>,
    target: Arc<UplinkTarget>,
    dry_run: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) {
    if !config.enabled {
        return;
    }
    // MAN-159: dry-run is the default, so say so once per target at
    // startup -- a safe default nobody can see is only half a fix. The
    // asymmetry in level is deliberate: the safe state is `info`, the
    // "this really is going out to a live target" state is `warn` so it
    // stands out in an operator's log. Per-target rather than one global
    // line in `main.rs`, because `dry_run` is a per-`[[rbn_uplink]]`
    // field and `start_spot_server` spawns one `serve` task per entry.
    // Goes through `tracing` (stderr, see `main.rs`'s subscriber setup)
    // so it can never interleave with `--json` spot output on stdout.
    if dry_run.load(Ordering::Relaxed) {
        tracing::info!(
            target_host = %config.target_host,
            target_port = config.target_port,
            spot_types = %config.spot_types.as_str(),
            "uplink: dry_run is ON (the default) -- connecting and logging in, \
             but NOT transmitting spots. Set `dry_run = false` in this \
             [[rbn_uplink]] block to transmit for real."
        );
    } else {
        tracing::warn!(
            target_host = %config.target_host,
            target_port = config.target_port,
            spot_types = %config.spot_types.as_str(),
            "uplink: dry_run = false -- transmitting real spots to this target."
        );
    }
    let login_callsign = config
        .effective_login_callsign(&station_callsign)
        .to_string();
    let mut backoff = INITIAL_BACKOFF;
    // MAN-67 (PR #80 review round 11): scoped to THIS target's own `serve`
    // invocation, never a process-wide static -- `main.rs` spawns one
    // independent `serve` task per configured `[[rbn_uplink]]` entry
    // specifically so one target's problems can't affect another's
    // delivery or retry timing. A shared static here would let a stuck
    // resolver on one target permanently WouldBlock every OTHER target
    // too, including an IP-literal one that never even needs DNS. `Arc`,
    // not a bare `Semaphore`, so `try_acquire_owned()` can hand the
    // spawned lookup task (see `connect_any_resolved_address`) a `'static`
    // permit -- a plain borrowed permit can't outlive this stack frame the
    // way a detached `tokio::spawn` task requires.
    let resolver_slot = Arc::new(Semaphore::new(1));

    loop {
        if *shutdown.borrow() {
            return;
        }

        match connect_and_forward(
            &config,
            &dry_run,
            &login_callsign,
            &bus,
            &target,
            &mut shutdown,
            &resolver_slot,
        )
        .await
        {
            Ok(()) => return, // clean shutdown-signaled exit
            Err(outcome) => {
                // No mark_disconnected() here: connect_and_forward already
                // paired its own mark_connected() with a mark_disconnected()
                // before returning Err (or never marked connected at all, if
                // it failed before login completed) -- this target's own
                // `connected` flag is otherwise untouched by this branch.
                target.record_reconnect();
                // `Disconnected` resets AND sleeps the reset value
                // immediately (MAN-44 code review CR-2): sleeping the
                // stale `backoff` first meant a healthy connection that
                // dropped after login still waited whatever an earlier,
                // unrelated outage had grown it to (up to `MAX_BACKOFF`)
                // before its first fast retry. `NeverConnected` keeps the
                // original ordering -- sleep the current rung, then grow
                // it -- so the never-connected ladder stays 1s/2s/4s/...
                let sleep_for = match outcome {
                    ConnectAttemptError::Disconnected => INITIAL_BACKOFF,
                    ConnectAttemptError::NeverConnected => backoff,
                };
                backoff = next_backoff(sleep_for, &outcome);
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Resolves `host:port` to every candidate address and gives each one its
/// own `CONNECT_TIMEOUT` attempt in turn (PR #80 review, round 2, finding
/// 2): `TcpStream::connect` alone tries every resolved address
/// internally, but a single timeout wrapped around the whole operation
/// can expire mid-attempt on the FIRST address, before Tokio ever
/// advances to a later, reachable one -- and since every retry
/// re-resolves and restarts from the first address again, a target whose
/// first resolved address black-holes SYNs (with a real one available
/// after it) could stay permanently unreachable. Returns the first
/// address that accepts, or the last error if every address failed or
/// timed out.
async fn connect_any_resolved_address(
    host: &str,
    port: u16,
    resolver_slot: &Arc<Semaphore>,
) -> std::io::Result<TcpStream> {
    // Bounded (PR #80 review, round 3): a bare `lookup_host` has no
    // timeout of its own -- a stalled system resolver would otherwise
    // leave this attempt stuck in the resolution phase for however long
    // the resolver takes, before `connect_first_reachable`'s own
    // per-address `CONNECT_TIMEOUT` ever gets a chance to apply. Shutdown
    // interruptibility is unaffected: this whole function is still one
    // arm of `connect_and_forward`'s outer `tokio::select!`, which races
    // it against `shutdown.changed()` regardless of where inside this
    // function execution currently is.
    //
    // The `timeout()` above only stops *this future* from waiting on the
    // result -- `lookup_host` runs the OS's blocking `getaddrinfo(3)` via
    // `spawn_blocking` internally, which has no cancellation mechanism and
    // keeps occupying/queuing a blocking-pool thread to completion
    // regardless (MAN-67, PR #80 review round 10). Left unbounded, a
    // sustained resolver stall means every reconnect attempt (each
    // eventually retried by `serve`'s backoff loop) piles another
    // abandoned lookup onto the blocking pool. `resolver_slot` (owned per
    // target -- see `serve`, PR #80 review round 11) bounds this to at
    // most one outstanding lookup PER TARGET at a time: the permit is
    // acquired here but held inside the spawned task until the real
    // `getaddrinfo` call actually finishes, so it survives this function
    // timing out or being dropped by the outer shutdown race. `_owned`:
    // the spawned task must be `'static`, which a permit borrowed from a
    // non-static `&Semaphore` can't satisfy.
    let Ok(permit) = resolver_slot.clone().try_acquire_owned() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "a previous DNS resolution for {host}:{port} is still outstanding; \
                 skipping this connect attempt rather than piling on another lookup"
            ),
        ));
    };
    let owned_host = host.to_string();
    let lookup = tokio::task::spawn(async move {
        let _permit = permit; // held until the blocking getaddrinfo job completes
        tokio::net::lookup_host((owned_host.as_str(), port))
            .await
            .map(|iter| iter.collect::<Vec<_>>())
    });
    let addrs: Vec<_> = match tokio::time::timeout(CONNECT_TIMEOUT, lookup).await {
        Ok(Ok(Ok(addrs))) => addrs,
        Ok(Ok(Err(e))) => return Err(e),
        Ok(Err(join_err)) => {
            return Err(std::io::Error::other(format!(
                "DNS resolution task for {host}:{port} panicked: {join_err}"
            )))
        }
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("resolving {host}:{port} timed out"),
            ))
        }
    };
    connect_first_reachable(addrs).await
}

/// Tries each address in `addrs`, in order, returning the first one that
/// accepts within `CONNECT_TIMEOUT`, or the last error if every address
/// failed, timed out, or the loop as a whole exceeded
/// `OVERALL_CONNECT_TIMEOUT` (MAN-67). Split out from
/// `connect_any_resolved_address` so the "keep trying later addresses"
/// behavior is testable directly against a caller-supplied address list,
/// without depending on real DNS resolving to more than one address.
async fn connect_first_reachable(addrs: Vec<std::net::SocketAddr>) -> std::io::Result<TcpStream> {
    connect_first_reachable_bounded(addrs, CONNECT_TIMEOUT, OVERALL_CONNECT_TIMEOUT, |addr| {
        TcpStream::connect(addr)
    })
    .await
}

/// `connect_first_reachable`'s real logic, with both timeouts AND the
/// per-address connect operation itself as parameters. The connect
/// operation is injectable (PR #80 review round 11) so the overall-deadline
/// behavior is unit-testable with a fake, instantly-controllable "hangs
/// forever" attempt under paused tokio time -- the original version dialed
/// real TEST-NET-1 (192.0.2.0/24) addresses to simulate a black hole, which
/// depends on undocumented host/network behavior: a host with no route to
/// that block (or a gateway that rejects it) gets an immediate
/// `NetworkUnreachable` instead of a hang, silently turning the deadline
/// test into a no-op everywhere that's true.
async fn connect_first_reachable_bounded<T, F, Fut>(
    addrs: Vec<std::net::SocketAddr>,
    per_addr_timeout: Duration,
    overall_timeout: Duration,
    connect: F,
) -> std::io::Result<T>
where
    F: Fn(std::net::SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let attempt_all = async {
        let mut last_err = None;
        for addr in addrs {
            match tokio::time::timeout(per_addr_timeout, connect(addr)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) => last_err = Some(e),
                Err(_) => {
                    last_err = Some(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("connect to {addr} timed out"),
                    ))
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses to try")
        }))
    };
    match tokio::time::timeout(overall_timeout, attempt_all).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "connecting to any candidate address exceeded the overall connect window",
        )),
    }
}

async fn connect_and_forward(
    config: &RbnUplinkConfig,
    dry_run: &AtomicBool,
    login_callsign: &str,
    bus: &Arc<SpotBus>,
    target: &Arc<UplinkTarget>,
    shutdown: &mut watch::Receiver<bool>,
    resolver_slot: &Arc<Semaphore>,
) -> Result<(), ConnectAttemptError> {
    // Raced against shutdown, not just time-bounded (MAN-58 comment
    // finding 1): a target that silently black-holes SYNs would otherwise
    // block shutdown for up to CONNECT_TIMEOUT (per address -- see
    // connect_any_resolved_address) even when shutdown fires immediately.
    // Looping rather than a single select arm: cancelling and retrying a
    // half-open `connect()` attempt has no partial state to lose (unlike
    // a buffered line read), so a spurious `changed()` wakeup with
    // `*shutdown.borrow() == false` just tries again.
    let stream = loop {
        tokio::select! {
            result = connect_any_resolved_address(config.target_host.as_str(), config.target_port, resolver_slot) => {
                match result {
                    Ok(stream) => break stream,
                    Err(_) => return Err(ConnectAttemptError::NeverConnected),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return Ok(());
                }
            }
        }
    };
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);

    // Subscribe before completing login, matching telnet.rs's own
    // "subscribe before handshake" rule -- a spot published mid-login
    // must not be lost to the broadcast channel's no-history semantics.
    let mut rx = bus.subscribe();

    // Bounded (MAX_LINE_BYTES cap, via bounded_io) AND raced against
    // shutdown (MAN-58 finding 1): the prior bare `.await` had neither --
    // a target that accepted the connection but never sent a line hung
    // this task indefinitely and ignored shutdown signals. Looping, not a
    // single select arm: `read_line_bounded`'s own contract guarantees
    // bytes already consumed survive a losing race un-cleared in
    // `prompt_line`, so a spurious `changed()` wakeup with
    // `*shutdown.borrow() == false` resumes the same line rather than
    // losing progress.
    let mut prompt_line = String::new();
    loop {
        tokio::select! {
            result = read_line_bounded_with_timeout(&mut reader, &mut prompt_line) => {
                if result.is_err() {
                    // Counted before propagating (PR #80 review, round
                    // 7): `rx` was already subscribed above (subscribe-
                    // before-handshake), so a stalled or errored login
                    // prompt still abandons whatever was published during
                    // the wait -- the next connection attempt subscribes
                    // fresh with no history.
                    record_disconnect_loss(target, &mut rx, 0, config.spot_types);
                }
                result.map_err(|_| ConnectAttemptError::NeverConnected)?;
                break;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    record_disconnect_loss(target, &mut rx, 0, config.spot_types);
                    return Ok(());
                }
            }
        }
    }
    if let Err(_e) = write_with_timeout(&mut wr, format!("{login_callsign}\r\n").as_bytes()).await {
        // A genuine write failure (PR #80 review, round 10, correcting
        // round 7's own fix): unlike the login-PROMPT READ just above,
        // this is the socket write itself failing/timing out, so it
        // belongs in `record_write_failure_loss` (the counter reserved
        // for actual write failures), not `record_disconnect_loss` --
        // `extra = 0` since there's no queued bus spot being sent here,
        // only the login line, but the backlog `rx` already accumulated
        // during the handshake is still abandoned.
        record_write_failure_loss(target, &mut rx, 0, config.spot_types);
        return Err(ConnectAttemptError::NeverConnected);
    }

    target.mark_connected();
    let result = forward_loop(
        &mut reader,
        &mut wr,
        &mut rx,
        dry_run,
        config.spot_types,
        login_callsign,
        bus,
        target,
        shutdown,
    )
    .await;
    target.mark_disconnected();
    result.map_err(|_| ConnectAttemptError::Disconnected)
}

/// RBN's collection server isn't expected to send anything meaningful
/// back after login, but this task must still poll the read half --
/// otherwise a remote-side close (FIN) is invisible until the next
/// spot happens to be published and the resulting write fails. A node
/// that only notices it's disconnected whenever the next spot arrives
/// could sit silently un-contributing for an arbitrarily long gap.
///
/// Records however many bus spots are being abandoned as a spot's write
/// to the target ITSELF just failed or timed out -- distinct from
/// `record_disconnect_loss` below (PR #80 review, round 8: conflating
/// every disconnect cause into the write-failure counter contradicted its
/// own name/HELP text and would misdirect alerting). `extra` is `1` for
/// a spot whose write failed, or `0` for a failed login write. Filtered
/// backlog entries are suppressed; only the remaining entries count as
/// write failures.
fn record_write_failure_loss(
    target: &UplinkTarget,
    rx: &mut broadcast::Receiver<crate::bus::BusSpot>,
    extra: u64,
    spot_types: UplinkSpotTypes,
) {
    let n = extra + count_unsuppressed_backlog(target, rx, spot_types);
    if n > 0 {
        target.record_write_failed(n);
    }
}

/// Records however many bus spots are being abandoned as the uplink
/// connection is torn down for a reason OTHER than a failed/timed-out
/// write itself -- a rate-limit disconnect, a protocol violation, a
/// stalled login prompt, or a shutdown cancelling an in-flight write. See
/// `record_write_failure_loss`'s doc comment for why these are two
/// separate counters, not one. `extra` is `1` when the triggering event
/// was itself a specific spot whose write was cancelled (not failed) in
/// flight, `0` when only the backlog is lost (no single spot to blame).
///
/// Every early return out of `forward_loop`'s select loop, and out of
/// `connect_and_forward`'s pre-login connect/prompt-read loops once `rx`
/// has been subscribed, MUST call one of these two helpers first (PR #80
/// review, rounds 3-8): this exact accounting gap recurred at a NEW exit
/// path across five consecutive review rounds -- the "same code region
/// keeps breaking" signal that ad-hoc inline `rx.len()` at each call site
/// was the wrong shape, not that any individual fix was wrong. Named
/// helpers make the correct call (into the correct counter) the path of
/// least resistance at any exit site added in the future.
fn record_disconnect_loss(
    target: &UplinkTarget,
    rx: &mut broadcast::Receiver<crate::bus::BusSpot>,
    extra: u64,
    spot_types: UplinkSpotTypes,
) {
    let n = extra + count_unsuppressed_backlog(target, rx, spot_types);
    if n > 0 {
        target.record_disconnected(n);
    }
}

/// Classify the backlog present when teardown starts. Bound the drain by
/// that snapshot so ongoing publishers cannot delay disconnect indefinitely.
/// Overwritten entries have no recoverable type and count as lagged; retained
/// entries excluded by the target's type policy count as suppressed.
fn count_unsuppressed_backlog(
    target: &UplinkTarget,
    rx: &mut broadcast::Receiver<crate::bus::BusSpot>,
    spot_types: UplinkSpotTypes,
) -> u64 {
    let mut remaining = rx.len() as u64;
    let mut unsuppressed = 0;
    while remaining > 0 {
        match rx.try_recv() {
            Ok(bus_spot) => {
                remaining -= 1;
                if spot_types.forwards(bus_spot.spot.spot_type) {
                    unsuppressed += 1;
                } else {
                    target.record_suppressed();
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                let lost = n.min(remaining);
                target.record_lagged(lost);
                remaining -= lost;
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break;
            }
        }
    }
    unsuppressed
}

#[allow(clippy::too_many_arguments)]
async fn forward_loop(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    rx: &mut broadcast::Receiver<crate::bus::BusSpot>,
    dry_run: &AtomicBool,
    spot_types: UplinkSpotTypes,
    spotter_call: &str,
    bus: &Arc<SpotBus>,
    target: &Arc<UplinkTarget>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut discard = String::new();
    let mut response_limiter =
        RateLimiter::new(MAX_TARGET_RESPONSE_LINES, TARGET_RESPONSE_RATE_WINDOW);
    loop {
        tokio::select! {
            recv = rx.recv() => {
                match recv {
                    Ok(bus_spot) => {
                        // MAN-91: RBN only takes CQ/TEST callers and
                        // beacons, so by default other types stop here.
                        // Local telnet/JSON have their own subscriptions
                        // and still see this spot. Its own parameter, not
                        // `config.spot_types`, so it stays independent of
                        // where `dry_run` is read from (MAN-78).
                        if !spot_types.forwards(bus_spot.spot.spot_type) {
                            target.record_suppressed();
                            continue;
                        }
                        if dry_run.load(Ordering::Relaxed) {
                            target.record_suppressed();
                            continue;
                        }
                        let unix_ts = bus.unix_ts_for(bus_spot.spot.sample_ts);
                        // MAN-88 Decision 1: the uplink always emits the RBN
                        // relay layout, independent of [server].line_format
                        // -- see that key's doc comment in config.rs.
                        let line = rbn::format_line(
                            &bus_spot.spot,
                            spotter_call,
                            unix_ts,
                            rbn::LineFormat::Rbn,
                        );
                        let wire_line = format!("{line}\r\n");
                        // Raced against shutdown too, not just bounded by
                        // WRITE_TIMEOUT (PR #80 review, round 3): once
                        // inside this branch, the outer select! is no
                        // longer polling its own `shutdown.changed()` arm
                        // -- without this inner race, a target that
                        // stopped reading would leave shutdown
                        // unobserved for up to the full WRITE_TIMEOUT
                        // (10s), which can exceed a service manager's
                        // graceful-shutdown window.
                        tokio::select! {
                            write_result = write_with_timeout(wr, wire_line.as_bytes()) => {
                                if write_result.is_err() {
                                    record_write_failure_loss(target, rx, 1, spot_types);
                                    write_result?;
                                }
                                target.record_sent();
                            }
                            _ = shutdown.changed() => {
                                if *shutdown.borrow() {
                                    record_disconnect_loss(target, rx, 1, spot_types);
                                    return Ok(());
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        target.record_lagged(n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        record_disconnect_loss(target, rx, 0, spot_types);
                        return Ok(());
                    }
                }
            }
            // Bounded via bounded_io (MAN-58 finding 2): an unterminated
            // long line from the remote used to grow `discard` without
            // bound while this read was pending. No idle timeout here,
            // unlike the login-prompt read -- this branch is already
            // covered by the surrounding `select!`'s shutdown race, and
            // this connection is expected to sit quietly with nothing to
            // read for long stretches (a live spot uplink, not a
            // request/response protocol), matching telnet.rs's own
            // established/logged-in-client read (round-5 review finding
            // there against reusing the timed variant post-login).
            read_result = read_line_bounded(reader, &mut discard) => {
                match read_result {
                    Ok(0) => {
                        record_disconnect_loss(target, rx, 0, spot_types);
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "RBN uplink target closed the connection",
                        ));
                    }
                    Ok(_) => {
                        discard.clear();
                        // Every completed response line counts against
                        // the budget (MAN-58 comment finding 3), whether
                        // or not the target was expected to send it --
                        // an unbounded stream of otherwise-harmless lines
                        // is still unbounded CPU/bandwidth work.
                        if !response_limiter.allow() {
                            record_disconnect_loss(target, rx, 0, spot_types);
                            return Err(std::io::Error::other(
                                "RBN uplink target exceeded the response-line rate budget",
                            ));
                        }
                    }
                    Err(e) => {
                        record_disconnect_loss(target, rx, 0, spot_types);
                        return Err(e);
                    }
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    record_disconnect_loss(target, rx, 0, spot_types);
                    return Ok(());
                }
            }
        }
    }
}

/// `wr.write_all` bounded by `WRITE_TIMEOUT` (MAN-58 comment finding 2),
/// matching `telnet.rs`'s identically-named helper for the inbound side.
async fn write_with_timeout(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    buf: &[u8],
) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, wr.write_all(buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "uplink write timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use std::net::TcpListener as StdTcpListener;

    fn sample_spot_for_loss_tests() -> manta_spot::Spot {
        manta_spot::Spot {
            callsign: "JA1ABC".to_string(),
            freq_hz: 14_027_100.0,
            snr_db: 23.0,
            wpm: 28.0,
            spot_type: manta_spot::SpotType::Cq,
            confidence: 0.9,
            track_id: 1,
            sample_ts: 0,
        }
    }

    #[test]
    fn teardown_separates_filtered_backlog_from_transport_loss() {
        for (spot_types, suppressed, lost) in [
            (UplinkSpotTypes::CqBeacon, 2, 3),
            (UplinkSpotTypes::All, 0, 5),
        ] {
            for write_failed in [false, true] {
                for lagged in [0, 1] {
                    let (tx, mut rx) = broadcast::channel(4);
                    let target = Metrics::new().register_uplink_target("t".to_string(), false);
                    // This optional entry is overwritten before teardown can inspect its type.
                    for _ in 0..lagged {
                        tx.send(crate::bus::BusSpot {
                            spot: sample_spot_for_loss_tests(),
                            occurrence_count: 1,
                        })
                        .unwrap();
                    }
                    for spot_type in [
                        manta_spot::SpotType::De,
                        manta_spot::SpotType::Unknown,
                        manta_spot::SpotType::Cq,
                        manta_spot::SpotType::Beacon,
                    ] {
                        tx.send(crate::bus::BusSpot {
                            spot: manta_spot::Spot {
                                spot_type,
                                ..sample_spot_for_loss_tests()
                            },
                            occurrence_count: 1,
                        })
                        .unwrap();
                    }
                    if write_failed {
                        record_write_failure_loss(&target, &mut rx, 1, spot_types);
                    } else {
                        record_disconnect_loss(&target, &mut rx, 1, spot_types);
                    }
                    assert_eq!(target.suppressed_total(), suppressed);
                    assert_eq!(target.lagged_total(), lagged);
                    assert_eq!(
                        target.write_failed_total(),
                        if write_failed { lost } else { 0 }
                    );
                    assert_eq!(
                        target.disconnected_total(),
                        if write_failed { 0 } else { lost }
                    );
                    assert!(rx.is_empty(), "teardown must consume the accounted backlog");
                }
            }
        }
    }

    /// PR #80 review, rounds 3-8: `record_write_failure_loss` and
    /// `record_disconnect_loss` are the two places every disconnect-
    /// causing exit from `forward_loop`/`connect_and_forward` must go
    /// through, after this exact accounting gap recurred at a new exit
    /// path across five consecutive review rounds, and round 8 further
    /// split "write itself failed" from "connection torn down for some
    /// other reason" into separate counters so
    /// `uplink_write_failed_total`'s own name/HELP text stays accurate.
    #[test]
    fn record_write_failure_loss_counts_extra_plus_backlog() {
        let epoch = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let bus = crate::bus::SpotBus::new(96_000.0, epoch, 0);
        let mut rx = bus.subscribe();
        let target = Metrics::new().register_uplink_target("t".to_string(), true);

        // 3 spots queued in the backlog, none yet drained by `rx`.
        let spot = sample_spot_for_loss_tests();
        bus.publish(spot.clone());
        bus.publish(spot.clone());
        bus.publish(spot);
        assert_eq!(rx.len(), 3);

        record_write_failure_loss(&target, &mut rx, 1, UplinkSpotTypes::default()); // the failed spot + backlog
        assert_eq!(target.write_failed_total(), 4);
        assert_eq!(
            target.disconnected_total(),
            0,
            "a write failure must not also count against the disconnect counter"
        );

        record_write_failure_loss(&target, &mut rx, 1, UplinkSpotTypes::default()); // only the new in-flight spot remains
        assert_eq!(target.write_failed_total(), 5);
    }

    #[test]
    fn record_disconnect_loss_counts_extra_plus_backlog_separately_from_write_failures() {
        let epoch = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let bus = crate::bus::SpotBus::new(96_000.0, epoch, 0);
        let mut rx = bus.subscribe();
        let target = Metrics::new().register_uplink_target("t".to_string(), true);

        let spot = sample_spot_for_loss_tests();
        bus.publish(spot.clone());
        bus.publish(spot);
        assert_eq!(rx.len(), 2);

        record_disconnect_loss(&target, &mut rx, 0, UplinkSpotTypes::default()); // e.g. a rate-limit disconnect: no single spot to blame
        assert_eq!(target.disconnected_total(), 2);
        assert_eq!(
            target.write_failed_total(),
            0,
            "a non-write disconnect must not also count against the write-failure counter"
        );

        record_disconnect_loss(&target, &mut rx, 1, UplinkSpotTypes::default()); // e.g. shutdown cancelling an in-flight write
        assert_eq!(target.disconnected_total(), 3);
    }

    #[test]
    fn record_loss_helpers_record_nothing_when_extra_and_backlog_are_both_zero() {
        let epoch = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let bus = crate::bus::SpotBus::new(96_000.0, epoch, 0);
        let mut rx = bus.subscribe();
        let target = Metrics::new().register_uplink_target("t".to_string(), true);

        record_write_failure_loss(&target, &mut rx, 0, UplinkSpotTypes::default());
        record_disconnect_loss(&target, &mut rx, 0, UplinkSpotTypes::default());
        assert_eq!(
            target.write_failed_total(),
            0,
            "must not record a spurious 0-count event"
        );
        assert_eq!(target.disconnected_total(), 0);
    }

    #[test]
    fn effective_login_callsign_reflected_in_config() {
        // Coverage for RbnUplinkConfig::effective_login_callsign itself
        // lives in config.rs; this just confirms the module wiring
        // compiles against the real type. Behavior is exercised by the
        // integration tests in tests/uplink_acceptance.rs.
        let cfg = RbnUplinkConfig {
            enabled: true,
            target_host: "example.invalid".to_string(),
            target_port: 7300,
            login_callsign: None,
            dry_run: false,
            spot_types: crate::config::UplinkSpotTypes::default(),
        };
        assert_eq!(cfg.effective_login_callsign("W3XYZ"), "W3XYZ");
    }

    /// PR #80 review, round 2, finding 2: if the FIRST address in the
    /// list can't be connected to, `connect_first_reachable` must still
    /// try the next one rather than giving up entirely -- the actual bug
    /// this fixes (a single timeout wrapped around `TcpStream::connect`'s
    /// own internal multi-address fallback could expire on address 1
    /// before Tokio ever reached a reachable address 2). Uses a refused
    /// connection (a bound-then-dropped port) for "unreachable" rather
    /// than a real SYN black-hole, which would need the full
    /// `CONNECT_TIMEOUT` to fail -- both are address-1-fails-immediately-
    /// try-address-2 from this function's perspective.
    #[tokio::test]
    async fn connect_first_reachable_falls_through_to_a_later_working_address() {
        let refused_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let refused_addr = refused_listener.local_addr().unwrap();
        drop(refused_listener); // now nothing listens on this port: connection refused

        let good_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let good_addr = good_listener.local_addr().unwrap();

        let accept_task = std::thread::spawn(move || good_listener.accept().unwrap());

        let stream = connect_first_reachable(vec![refused_addr, good_addr])
            .await
            .expect("must fall through to the second, reachable address");
        assert_eq!(stream.peer_addr().unwrap(), good_addr);

        accept_task.join().unwrap();
    }

    #[tokio::test]
    async fn connect_first_reachable_errors_when_every_address_fails() {
        let refused_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let refused_addr = refused_listener.local_addr().unwrap();
        drop(refused_listener);

        let err = connect_first_reachable(vec![refused_addr])
            .await
            .expect_err("every address failing must be a real error, not a silent success");
        assert_ne!(err.kind(), std::io::ErrorKind::NotFound); // a real connect error, not the empty-list fallback
    }

    /// MAN-67, PR #80 review round 9 (and round 11's follow-up on the
    /// original test's host-dependence): a long list of black-holed
    /// candidate addresses must not grant the loop an unbounded total
    /// budget (`address_count * per_addr_timeout`) -- it must give up once
    /// `overall_timeout` elapses, even with addresses left untried. Uses a
    /// fake `connect` that never resolves on its own (`std::future::pending`)
    /// under paused tokio time, instead of dialing real TEST-NET-1
    /// addresses: a real black-hole target depends on undocumented
    /// host/network behavior (a host with no route to that block, or a
    /// gateway that rejects it, gets an immediate `NetworkUnreachable`
    /// instead of a hang, which would silently turn this test into a
    /// no-op). The fake hangs unconditionally, so this test exercises the
    /// deadline logic itself regardless of the host's own networking.
    #[tokio::test(start_paused = true)]
    async fn connect_first_reachable_bounded_stops_at_the_overall_deadline() {
        let per_addr_timeout = Duration::from_secs(10);
        // Between 1x and 2x per_addr_timeout: address 1 exhausts its own
        // 10s budget at t=10s, address 2 starts then -- but the overall
        // deadline at t=15s cuts address 2 off 5s into its own budget,
        // before address 3 ever starts (which would begin at t=20s).
        let overall_timeout = Duration::from_secs(15);
        let addrs: Vec<std::net::SocketAddr> = vec![
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
            "127.0.0.1:3".parse().unwrap(),
        ]; // never actually dialed -- `connect` below is faked, so the
           // specific addresses are only distinct placeholders
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_for_connect = attempts.clone();

        let connect = move |_addr: std::net::SocketAddr| {
            let attempts = attempts_for_connect.clone();
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<std::io::Result<()>>().await
            }
        };

        let started = tokio::time::Instant::now();
        let result: std::io::Result<()> =
            connect_first_reachable_bounded(addrs, per_addr_timeout, overall_timeout, connect)
                .await;
        let elapsed = started.elapsed();
        let err = result.expect_err("every address hanging forever must still be a real error");

        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            elapsed < per_addr_timeout * 3,
            "overall_timeout should have cut the loop off before every address was tried \
             (elapsed {elapsed:?}, would-be full per-address budget {:?})",
            per_addr_timeout * 3
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the third address must never have been attempted -- the overall deadline should \
             have cut the loop off mid-way through the second address's own per-address budget"
        );
    }

    /// MAN-67, PR #80 review round 10: an outstanding (still-running)
    /// blocking resolver job must block a NEW lookup from starting, so a
    /// sustained resolver stall can never pile up more than one abandoned
    /// `getaddrinfo` job on the blocking pool, however many times
    /// `serve`'s backoff loop retries in the meantime. Holds the slot's
    /// only permit directly (no real DNS activity needed, and none happens
    /// here: the early `WouldBlock` return fires before
    /// `connect_any_resolved_address` ever calls `lookup_host`) -- a fresh
    /// `Semaphore` local to this test, not the shared production one (round
    /// 11 made the real resolver slot per-target rather than a shared
    /// static, so there's no longer a process-wide instance to contend
    /// over here either).
    #[tokio::test]
    async fn connect_any_resolved_address_skips_a_new_lookup_while_one_is_outstanding() {
        let resolver_slot = Arc::new(Semaphore::new(1));
        let permit = resolver_slot
            .clone()
            .try_acquire_owned()
            .expect("a freshly created semaphore must have its permit available");

        let err = connect_any_resolved_address("example.invalid", 7300, &resolver_slot)
            .await
            .expect_err("must not start a second lookup while one is outstanding");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);

        drop(permit);
    }

    fn cfg(host: &str, port: u16) -> RbnUplinkConfig {
        RbnUplinkConfig {
            enabled: true,
            target_host: host.to_string(),
            target_port: port,
            login_callsign: None,
            dry_run: false,
            spot_types: crate::config::UplinkSpotTypes::default(),
        }
    }

    /// MAN-128 D7: labels are `host:port`, with `#N` appended to the 2nd and
    /// later exact duplicate of the same `host:port` -- deterministic
    /// across calls, matching PR #95 (MAN-44)'s convention.
    #[test]
    fn target_labels_are_host_port_with_suffix_only_on_duplicates() {
        let configs = vec![cfg("a", 1), cfg("b", 2), cfg("a", 1), cfg("a", 1)];
        assert_eq!(
            target_labels(&configs),
            vec!["a:1", "b:2", "a:1#2", "a:1#3"]
        );
        // Deterministic across repeated calls against the same input.
        assert_eq!(
            target_labels(&configs),
            vec!["a:1", "b:2", "a:1#2", "a:1#3"]
        );
    }
}
