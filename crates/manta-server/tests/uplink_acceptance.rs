//! MAN-32 acceptance scenarios:
//!   Scenario: manta pushes its own spots to the RBN's collection endpoint
//!     Given manta is configured with RBN node credentials/target
//!     When manta validates and emits a spot
//!     Then the spot is forwarded to the RBN's own spot-collection endpoint
//!     And the forwarded spot uses the format RBN's ingestion expects
//!
//!   Scenario: A dry-run configuration suppresses the actual RBN send
//!     Given manta is configured with RBN node credentials/target
//!     And the outbound connection is set to dry-run mode
//!     When manta validates and emits a spot
//!     Then the spot is NOT forwarded to the RBN's collection endpoint
//!     And the spot is still visible through manta's local telnet/JSON output
//!
//! MAN-42 acceptance scenarios:
//!   Scenario: Spots are forwarded to every configured RBN target
//!     Given manta's uplink is configured with two RBN collection targets
//!     When manta validates and emits a spot
//!     Then the spot is forwarded to both configured targets
//!
//!   Scenario: One target failing does not stop delivery to the others
//!     Given manta's uplink is configured with two RBN collection targets
//!     And one target's connection is down
//!     When manta validates and emits a spot
//!     Then the spot is still forwarded to the target that is reachable
//!     And the unreachable target's connection is retried independently
//!
//! MAN-159 acceptance scenario:
//!   Scenario: Enabling the uplink without an explicit dry_run setting does not transmit
//!     Given an operator adds a [[rbn_uplink]] block with no dry_run key at all
//!     When manta starts
//!     Then it does not transmit spots to the configured target
//!
//! MAN-91 acceptance scenarios:
//!   Scenario: A CQ spot is forwarded
//!     Given manta validates a spot classified as a CQing station
//!     When the RBN uplink is enabled with its default configuration
//!     Then the spot is forwarded to the configured RBN target
//!
//!   Scenario: A DE (answering-station) spot is not forwarded by default
//!     Given manta validates a spot classified as a station answering another call (type De)
//!     When the RBN uplink is enabled with its default configuration
//!     Then the spot is not forwarded to RBN, though it remains visible on
//!       manta's local telnet/JSON output
//!
//!   Scenario: The default is overridable
//!     Given an operator wants to forward every spot type regardless of RBN's convention
//!     When they set the uplink's spot-type filter to "all"
//!     Then every validated spot is forwarded
//!
//! The mock listener in this file stands in for RBN's own collection
//! server -- manta is the connecting *client* here, the reverse of
//! `telnet_acceptance.rs`'s role.

use manta_server::bus::SpotBus;
use manta_server::config::{RbnUplinkConfig, UplinkSpotTypes};
use manta_server::metrics::Metrics;
use manta_server::rbn;
use manta_spot::{Spot, SpotType};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const SAMPLE_RATE_HZ: f64 = 96_000.0;
const STATION_CALL: &str = "W3XYZ";

fn sample_spot() -> Spot {
    Spot {
        callsign: "JA1ABC".to_string(),
        freq_hz: 14_027_100.0,
        snr_db: 23.0,
        wpm: 28.0,
        spot_type: SpotType::Cq,
        confidence: 0.9,
        track_id: 1,
        sample_ts: 0,
    }
}

fn uplink_config(target_port: u16, dry_run: bool) -> RbnUplinkConfig {
    RbnUplinkConfig {
        enabled: true,
        target_host: "127.0.0.1".to_string(),
        target_port,
        login_callsign: None,
        dry_run,
        spot_types: UplinkSpotTypes::default(),
    }
}

/// One spot per `SpotType`, each with its own callsign, in the order the
/// MAN-91 tests publish them: the two RBN holds back by default first.
fn one_spot_per_type() -> Vec<Spot> {
    [
        ("JA1DE", SpotType::De),
        ("JA1UNK", SpotType::Unknown),
        ("JA1CQ", SpotType::Cq),
        ("JA1BCN", SpotType::Beacon),
    ]
    .into_iter()
    .map(|(call, spot_type)| Spot {
        callsign: call.to_string(),
        spot_type,
        ..sample_spot()
    })
    .collect()
}

/// Reads one line off the mock target, failing the test after 5 s.
async fn read_forwarded_line(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for a forwarded spot line")
        .unwrap();
    line.trim_end().to_string()
}

struct Harness {
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

/// Spawns `uplink::serve` against `target_port` (a mock RBN listener the
/// test itself controls) and returns the shared bus/metrics/shutdown
/// handles the test drives.
fn spawn_uplink(target_port: u16, dry_run: bool) -> Harness {
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let cfg = uplink_config(target_port, dry_run);
    let target = metrics.register_uplink_target(format!("127.0.0.1:{target_port}"), true);
    let bus2 = bus.clone();
    tokio::spawn(async move {
        manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus2, target, shutdown_rx).await;
    });

    Harness {
        bus,
        metrics,
        shutdown_tx,
    }
}

/// Spawns one `uplink::serve` task per config, all sharing the same
/// bus/metrics/shutdown -- mirroring `start_spot_server`'s real MAN-42
/// wiring (one independent task per configured `[[rbn_uplink]]` target).
fn spawn_uplinks(configs: Vec<RbnUplinkConfig>) -> Harness {
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let labels = manta_server::uplink::target_labels(&configs);
    for (cfg, label) in configs.into_iter().zip(labels) {
        let target = metrics.register_uplink_target(label, cfg.enabled);
        let bus2 = bus.clone();
        let shutdown_rx2 = shutdown_rx.clone();
        tokio::spawn(async move {
            manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus2, target, shutdown_rx2)
                .await;
        });
    }

    Harness {
        bus,
        metrics,
        shutdown_tx,
    }
}

/// Accepts one connection on `listener`, performs the login side of the
/// handshake (send a login prompt, read back the client's login line),
/// and returns the login line plus the still-open reader/writer for the
/// test to keep asserting on.
async fn mock_rbn_accept_and_login(
    listener: &TcpListener,
) -> (
    String,
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let (socket, _peer) = listener.accept().await.unwrap();
    let (rd, mut wr) = socket.into_split();
    let mut reader = BufReader::new(rd);

    wr.write_all(b"login: \r\n").await.unwrap();
    let mut login_line = String::new();
    reader.read_line(&mut login_line).await.unwrap();

    (login_line, reader, wr)
}

#[tokio::test]
async fn logs_in_and_forwards_a_published_spot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    assert_eq!(login_line.trim_end(), STATION_CALL);

    let spot = sample_spot();
    let expected = rbn::format_line(
        &spot,
        STATION_CALL,
        harness.bus.unix_ts_for(spot.sample_ts),
        rbn::LineFormat::Rbn,
    );
    harness.bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for the forwarded spot line")
        .unwrap();

    assert_eq!(line.trim_end(), expected);
    assert_eq!(harness.metrics.uplink_sent_total(), 1);
    assert!(harness.metrics.uplink_connected());

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn dry_run_logs_in_but_does_not_forward_the_spot_line() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), true);

    let (login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    assert_eq!(
        login_line.trim_end(),
        STATION_CALL,
        "dry-run must still complete the login handshake"
    );

    harness.bus.publish(sample_spot());

    let mut line = String::new();
    let result =
        tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await;
    assert!(
        result.is_err(),
        "dry-run must not transmit the spot line, got: {line:?}"
    );
    assert_eq!(harness.metrics.uplink_sent_total(), 0);
    assert_eq!(harness.metrics.uplink_suppressed_total(), 1);

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-78: a live reload flips `dry_run` through the shared flag; the
/// next spot honours it on the same connection, with no reconnect.
#[tokio::test]
async fn dry_run_flag_flipped_while_connected_applies_to_the_next_spot_without_reconnecting() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let target = metrics.register_uplink_target(format!("127.0.0.1:{port}"), true);
    let dry_run = Arc::new(AtomicBool::new(true));
    // `config.dry_run` deliberately disagrees with the flag: once running,
    // only the flag may be read.
    let cfg = uplink_config(port, false);
    tokio::spawn(manta_server::uplink::serve_with_live_dry_run(
        cfg,
        STATION_CALL.to_string(),
        bus.clone(),
        target.clone(),
        dry_run.clone(),
        shutdown_rx,
    ));

    let (login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    assert_eq!(login_line.trim_end(), STATION_CALL);

    bus.publish(sample_spot());
    let mut line = String::new();
    let result =
        tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await;
    assert!(
        result.is_err(),
        "dry_run flag on: nothing may be sent, got {line:?}"
    );
    assert_eq!(target.suppressed_total(), 1);

    dry_run.store(false, Ordering::Relaxed);
    let spot = sample_spot();
    let expected = rbn::format_line(
        &spot,
        STATION_CALL,
        bus.unix_ts_for(spot.sample_ts),
        rbn::LineFormat::Rbn,
    );
    bus.publish(spot);
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for the spot line after dry_run was turned off")
        .unwrap();
    assert_eq!(
        line.trim_end(),
        expected,
        "must arrive on the same connection"
    );

    dry_run.store(true, Ordering::Relaxed);
    bus.publish(sample_spot());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while target.suppressed_total() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the third spot was never suppressed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut more = String::new();
    let result =
        tokio::time::timeout(Duration::from_millis(300), reader.read_line(&mut more)).await;
    assert!(
        result.is_err(),
        "dry_run back on: nothing more may be sent, got {more:?}"
    );
    assert_eq!(target.reconnects_total(), 0);
    assert_eq!(metrics.uplink_sent_total(), 1);

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn dry_run_does_not_affect_other_bus_subscribers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), true);
    let (_login_line, _reader, _wr) = mock_rbn_accept_and_login(&listener).await;

    // A second, independent subscriber standing in for the telnet/JSON
    // servers -- dry-run must be local to the uplink task, never a
    // bus-wide suppression.
    let mut other_rx = harness.bus.subscribe();
    harness.bus.publish(sample_spot());

    let received = tokio::time::timeout(Duration::from_secs(5), other_rx.recv())
        .await
        .expect("other subscriber timed out")
        .unwrap();
    assert_eq!(received.spot.callsign, "JA1ABC");

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn reconnects_after_the_remote_end_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    // First connection: accept, complete login, then drop it immediately
    // (simulating an RBN-side disconnect).
    let (_login_line, _reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    // `_reader`/`_wr` drop here, closing the socket.
    drop(_reader);
    drop(_wr);

    // The uplink must come back and log in again.
    let (login_line2, _reader2, _wr2) =
        tokio::time::timeout(Duration::from_secs(5), mock_rbn_accept_and_login(&listener))
            .await
            .expect("uplink did not reconnect in time");
    assert_eq!(login_line2.trim_end(), STATION_CALL);
    assert!(harness.metrics.uplink_reconnects_total() >= 1);

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn shutdown_signal_stops_the_reconnect_loop_without_a_new_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, _reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    drop(_reader);
    drop(_wr);

    // Signal shutdown immediately -- before asserting on a reconnect --
    // so the reconnect loop should observe it during its backoff sleep
    // and stop, rather than accepting a second connection.
    let _ = harness.shutdown_tx.send(true);

    let second_attempt = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
    assert!(
        second_attempt.is_err(),
        "uplink attempted to reconnect after shutdown was signaled"
    );
}

/// MAN-58 finding 1: a target that accepts the connection but never sends
/// a login prompt line used to hang this task's `read_line` indefinitely
/// and ignore shutdown. The bounded, shutdown-raced replacement must
/// close the connection promptly once shutdown fires, even mid-wait.
#[tokio::test]
async fn shutdown_signal_interrupts_a_hanging_login_prompt_read() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (socket, _peer) = listener.accept().await.unwrap();
    let (rd, _wr) = socket.into_split();
    let mut reader = BufReader::new(rd);
    // Deliberately never send a login prompt.

    let _ = harness.shutdown_tx.send(true);

    let mut buf = String::new();
    let read_result = tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut buf))
        .await
        .expect(
            "uplink did not close the connection promptly after shutdown while \
             still waiting on the login prompt",
        );
    assert_eq!(
        read_result.unwrap(),
        0,
        "expected EOF (client closed) after shutdown, not more data"
    );
}

/// MAN-58 finding 2: an unterminated response line from the target past
/// `bounded_io`'s length cap used to grow the discard buffer without
/// bound. It must instead be rejected, tearing down the connection (and
/// therefore triggering the normal reconnect/backoff path) rather than
/// hanging or leaking memory.
#[tokio::test]
async fn an_oversized_unterminated_response_line_forces_a_reconnect_instead_of_hanging() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, _reader, mut wr) = mock_rbn_accept_and_login(&listener).await;

    // Past bounded_io::MAX_LINE_BYTES (1024), no newline.
    wr.write_all(&vec![b'A'; 2000]).await.unwrap();

    let reconnected = tokio::time::timeout(Duration::from_secs(5), async {
        while harness.metrics.uplink_reconnects_total() < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        reconnected.is_ok(),
        "an oversized unterminated response line must force a reconnect, not hang forever"
    );

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-58 comment finding 3: with no rate budget on the target's post-
/// login response reads, a misbehaving or MITM'd target sending an
/// endless stream of short, valid, newline-terminated lines could keep
/// the uplink task hot indefinitely. A flood past the budget must instead
/// tear the connection down.
#[tokio::test]
async fn a_flood_of_short_response_lines_past_the_rate_budget_forces_a_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, _reader, mut wr) = mock_rbn_accept_and_login(&listener).await;

    // Comfortably past the response-line rate budget within one window --
    // individually harmless lines, unbounded only in aggregate.
    for _ in 0..40 {
        wr.write_all(b"noise\r\n").await.unwrap();
    }

    let reconnected = tokio::time::timeout(Duration::from_secs(5), async {
        while harness.metrics.uplink_reconnects_total() < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        reconnected.is_ok(),
        "a flood of response lines past the rate budget must force a reconnect"
    );

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn disabled_uplink_makes_no_connection_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Metrics::new();
    let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let mut cfg = uplink_config(addr.port(), false);
    cfg.enabled = false;
    let target = metrics.register_uplink_target(format!("127.0.0.1:{}", addr.port()), false);
    tokio::spawn(async move {
        manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus, target, shutdown_rx).await;
    });

    let attempt = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(
        attempt.is_err(),
        "a disabled uplink must never attempt a connection"
    );
}

/// MAN-159 acceptance scenario:
///   Scenario: Enabling the uplink without an explicit dry_run setting does not transmit
///     Given an operator adds a [[rbn_uplink]] block with no dry_run key at all
///     When manta starts
///     Then it does not transmit spots to the configured target
///
/// Goes through the real `DaemonConfigFile`/`toml::from_str` path on purpose:
/// `uplink_config()` sets `dry_run` explicitly, so it is structurally blind to
/// the serde default this test exists to protect.
#[tokio::test]
async fn omitted_dry_run_key_logs_in_but_does_not_transmit() {
    use manta_server::config::DaemonConfigFile;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let toml_src = format!(
        r#"
        [server]
        station_callsign = "W3XYZ"
        [[rbn_uplink]]
        enabled = true
        target_host = "127.0.0.1"
        target_port = {}
        "#,
        addr.port()
    );
    let file: DaemonConfigFile = toml::from_str(&toml_src).unwrap();
    let cfg = file.rbn_uplink[0].clone();
    assert!(cfg.dry_run, "an omitted dry_run key must default to true");

    let epoch = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let bus = Arc::new(SpotBus::new(SAMPLE_RATE_HZ, epoch, 0));
    let metrics = Arc::new(Metrics::new());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let target = metrics.register_uplink_target(format!("127.0.0.1:{}", addr.port()), true);
    let bus2 = bus.clone();
    tokio::spawn(async move {
        manta_server::uplink::serve(cfg, STATION_CALL.to_string(), bus2, target, shutdown_rx).await;
    });

    // The login handshake still completes -- dry-run gates the spot write
    // only, so operators can still validate connectivity.
    let (login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;
    assert_eq!(login_line.trim_end(), STATION_CALL);

    bus.publish(sample_spot());

    let mut line = String::new();
    let result =
        tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await;
    assert!(result.is_err(), "must not transmit, got: {line:?}");
    assert_eq!(metrics.uplink_sent_total(), 0);
    assert_eq!(metrics.uplink_suppressed_total(), 1);

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn spot_is_forwarded_to_every_configured_target() {
    let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port1 = listener1.local_addr().unwrap().port();
    let port2 = listener2.local_addr().unwrap().port();

    let harness = spawn_uplinks(vec![
        uplink_config(port1, false),
        uplink_config(port2, false),
    ]);

    let (login1, mut reader1, _wr1) = mock_rbn_accept_and_login(&listener1).await;
    let (login2, mut reader2, _wr2) = mock_rbn_accept_and_login(&listener2).await;
    assert_eq!(login1.trim_end(), STATION_CALL);
    assert_eq!(login2.trim_end(), STATION_CALL);

    let spot = sample_spot();
    let expected = rbn::format_line(
        &spot,
        STATION_CALL,
        harness.bus.unix_ts_for(spot.sample_ts),
        rbn::LineFormat::Rbn,
    );
    harness.bus.publish(spot);

    let mut line1 = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader1.read_line(&mut line1))
        .await
        .expect("timed out waiting for the forwarded spot on target 1")
        .unwrap();
    let mut line2 = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader2.read_line(&mut line2))
        .await
        .expect("timed out waiting for the forwarded spot on target 2")
        .unwrap();

    assert_eq!(line1.trim_end(), expected);
    assert_eq!(line2.trim_end(), expected);

    let _ = harness.shutdown_tx.send(true);
}

#[tokio::test]
async fn one_target_down_does_not_block_delivery_to_the_reachable_target_and_retries_independently()
{
    let listener_up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_up = listener_up.local_addr().unwrap().port();

    // Bind then immediately drop to get a port nothing listens on, so the
    // second uplink task's connection attempts fail for the life of the test.
    let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_down = temp.local_addr().unwrap().port();
    drop(temp);

    let harness = spawn_uplinks(vec![
        uplink_config(port_up, false),
        uplink_config(port_down, false),
    ]);

    // Then the spot is still forwarded to the target that is reachable
    let (login_up, mut reader_up, _wr_up) = mock_rbn_accept_and_login(&listener_up).await;
    assert_eq!(login_up.trim_end(), STATION_CALL);

    let spot = sample_spot();
    let expected = rbn::format_line(
        &spot,
        STATION_CALL,
        harness.bus.unix_ts_for(spot.sample_ts),
        rbn::LineFormat::Rbn,
    );
    harness.bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader_up.read_line(&mut line))
        .await
        .expect("timed out waiting for the forwarded spot on the reachable target")
        .unwrap();
    assert_eq!(line.trim_end(), expected);

    // And the unreachable target's connection is retried independently --
    // wait for at least one reconnect attempt from the down target's own
    // backoff loop.
    tokio::time::timeout(Duration::from_secs(5), async {
        while harness.metrics.uplink_reconnects_total() < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the unreachable target's reconnect loop never attempted a retry");

    // The down target's failed attempts must not clear the shared
    // uplink_connected gauge while the reachable target is still up
    // (regression coverage: a shared last-writer-wins boolean would flip
    // this to false here even though the reachable target never dropped).
    assert!(
        harness.metrics.uplink_connected(),
        "an unrelated target's failed reconnects cleared the connected gauge"
    );

    // The reachable target's own delivery must be unaffected by the other
    // target's ongoing retries: forward a second spot and confirm it still
    // arrives on the same still-open connection.
    let spot2 = sample_spot();
    let expected2 = rbn::format_line(
        &spot2,
        STATION_CALL,
        harness.bus.unix_ts_for(spot2.sample_ts),
        rbn::LineFormat::Rbn,
    );
    harness.bus.publish(spot2);
    let mut line2 = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader_up.read_line(&mut line2))
        .await
        .expect("reachable target stopped receiving spots while the other target retried")
        .unwrap();
    assert_eq!(line2.trim_end(), expected2);

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-128 Scenario 4: with two targets, one connected and one stuck
/// reconnecting, an operator must be able to tell WHICH target is down from
/// labeled per-target series, not just the aggregate count.
#[tokio::test]
async fn two_targets_one_down_are_distinguishable_per_target() {
    let listener_up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_up = listener_up.local_addr().unwrap().port();

    let temp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_down = temp.local_addr().unwrap().port();
    drop(temp);

    let harness = spawn_uplinks(vec![
        uplink_config(port_up, false),
        uplink_config(port_down, false),
    ]);

    let (login_up, _reader_up, _wr_up) = mock_rbn_accept_and_login(&listener_up).await;
    assert_eq!(login_up.trim_end(), STATION_CALL);

    tokio::time::timeout(Duration::from_secs(5), async {
        while harness.metrics.uplink_reconnects_total() < 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the down target never attempted a retry");

    let text = harness.metrics.render_prometheus_text();
    assert!(text.contains(&format!(
        r#"manta_uplink_target_connected{{target="127.0.0.1:{port_up}"}} 1"#
    )));
    assert!(text.contains(&format!(
        r#"manta_uplink_target_connected{{target="127.0.0.1:{port_down}"}} 0"#
    )));
    assert!(text.contains(&format!(
        r#"manta_uplink_target_reconnects_total{{target="127.0.0.1:{port_up}"}} 0"#
    )));

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-88 Decision 1: `[server].line_format` does not reach the uplink --
/// `RbnUplinkConfig` has no `line_format` field of its own, and
/// `forward_loop` pins `rbn::LineFormat::Rbn` unconditionally. Asserted
/// against literal columns (mode column present, time at column 71), the
/// bytes actually read back off the mock target's socket, not just the
/// return value of `rbn::format_line` the other tests in this file build
/// their expectation from.
#[tokio::test]
async fn the_uplink_always_emits_the_rbn_layout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;

    let spot = sample_spot();
    let unix_ts = harness.bus.unix_ts_for(spot.sample_ts);
    harness.bus.publish(spot);

    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("timed out waiting for the forwarded spot line")
        .unwrap();
    let line = line.trim_end();

    assert_eq!(line.find("CW").unwrap() + 1, 42, "line was: {line:?}");
    let secs_of_day = unix_ts.rem_euclid(86_400);
    let zulu = format!("{:02}{:02}Z", secs_of_day / 3600, (secs_of_day % 3600) / 60);
    assert_eq!(line.find(&zulu).unwrap() + 1, 71, "line was: {line:?}");

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-91 scenarios 1 and 2: with `spot_types` left at its default, the CQ
/// and beacon spots go out and the DE and untyped spots do not. Delivery to
/// one target is in order, so reading the CQ line first proves the two
/// earlier spots were never written.
#[tokio::test]
async fn default_spot_types_forwards_cq_and_beacon_but_not_de_or_untyped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);

    let (_login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;

    let spots = one_spot_per_type();
    let expected: Vec<String> = spots
        .iter()
        .filter(|s| matches!(s.spot_type, SpotType::Cq | SpotType::Beacon))
        .map(|s| {
            rbn::format_line(
                s,
                STATION_CALL,
                harness.bus.unix_ts_for(s.sample_ts),
                rbn::LineFormat::Rbn,
            )
        })
        .collect();
    for spot in spots {
        harness.bus.publish(spot);
    }

    let first = read_forwarded_line(&mut reader).await;
    let second = read_forwarded_line(&mut reader).await;
    assert_eq!(
        first, expected[0],
        "the CQ spot must be the first line sent"
    );
    assert!(
        first.contains("JA1CQ") && first.contains(" CQ "),
        "line was: {first:?}"
    );
    assert_eq!(
        second, expected[1],
        "the beacon spot must be the second line sent"
    );
    assert!(
        second.contains("JA1BCN") && second.contains("BEACON"),
        "line was: {second:?}"
    );

    assert_eq!(harness.metrics.uplink_sent_total(), 2);
    assert_eq!(
        harness.metrics.uplink_suppressed_total(),
        2,
        "the DE and untyped spots must be counted as suppressed, not dropped silently"
    );

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-91 scenario 3: `spot_types = "all"` forwards every type, DE and
/// untyped included, in publish order.
#[tokio::test]
async fn spot_types_all_forwards_every_type() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut cfg = uplink_config(addr.port(), false);
    cfg.spot_types = UplinkSpotTypes::All;
    let harness = spawn_uplinks(vec![cfg]);

    let (_login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;

    let spots = one_spot_per_type();
    let expected: Vec<String> = spots
        .iter()
        .map(|s| {
            rbn::format_line(
                s,
                STATION_CALL,
                harness.bus.unix_ts_for(s.sample_ts),
                rbn::LineFormat::Rbn,
            )
        })
        .collect();
    for spot in spots {
        harness.bus.publish(spot);
    }

    let mut received = Vec::new();
    for _ in 0..expected.len() {
        received.push(read_forwarded_line(&mut reader).await);
    }
    assert_eq!(received, expected);
    assert!(received[0].contains("JA1DE") && received[0].contains(" DE "));
    assert!(received[1].contains("JA1UNK"));

    assert_eq!(harness.metrics.uplink_sent_total(), 4);
    assert_eq!(harness.metrics.uplink_suppressed_total(), 0);

    let _ = harness.shutdown_tx.send(true);
}

/// MAN-91 scenario 2's second half: a DE spot the uplink holds back must
/// still reach every other bus subscriber -- the telnet/JSON servers each
/// take their own subscription, which this second receiver stands in for.
#[tokio::test]
async fn a_filtered_de_spot_still_reaches_other_bus_subscribers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let harness = spawn_uplink(addr.port(), false);
    let (_login_line, mut reader, _wr) = mock_rbn_accept_and_login(&listener).await;

    let mut other_rx = harness.bus.subscribe();
    let de_spot = Spot {
        spot_type: SpotType::De,
        ..sample_spot()
    };
    harness.bus.publish(de_spot);

    let received = tokio::time::timeout(Duration::from_secs(5), other_rx.recv())
        .await
        .expect("other subscriber timed out")
        .unwrap();
    assert_eq!(received.spot.callsign, "JA1ABC");
    assert_eq!(received.spot.spot_type, SpotType::De);

    let mut line = String::new();
    let result =
        tokio::time::timeout(Duration::from_millis(500), reader.read_line(&mut line)).await;
    assert!(
        result.is_err(),
        "the default uplink must not transmit a DE spot, got: {line:?}"
    );
    assert_eq!(harness.metrics.uplink_sent_total(), 0);
    assert_eq!(harness.metrics.uplink_suppressed_total(), 1);

    let _ = harness.shutdown_tx.send(true);
}
