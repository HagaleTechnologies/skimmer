# MAN-85: one signal path for SIGINT, SIGTERM and SIGHUP

`manta listen` registers exactly one signal handler, in
`crates/manta-cli/src/main.rs`, via `ctrlc::set_handler`. Until MAN-85 the
workspace declared `ctrlc = "3"` with no features, and `ctrlc`'s SIGTERM/
SIGHUP `sigaction` calls are compiled in only under its `termination`
feature (`ctrlc-3.5.2/src/platform/unix/mod.rs`). SIGTERM therefore kept
the OS default disposition: the kernel killed the process before
`manta_engine::listen`'s `stop` flag could be observed, so the entire
shutdown sequence -- `shutdown_tx.send(true)`, each client task's
drain-and-exit branch, `tasks::await_all` under `SHUTDOWN_DRAIN_DEADLINE`
-- never ran. Measured on `e398d46`, plain `listen`: SIGINT exit 0 in
18 ms; SIGTERM exit 143 in 11 ms. Every real service manager (systemd,
Docker, Kubernetes) sends SIGTERM on stop.

## Options considered

1. **Enable `ctrlc`'s `termination` feature.** One line in the workspace
   `Cargo.toml`; no `main.rs` change, because the existing handler closure
   already treats "the registered signal fired" as one undifferentiated
   event.
2. **Move to `tokio::signal::unix::signal`** with SIGTERM and SIGINT both
   wired to the stop flag, as the ticket's technical notes suggest.

## Decision: option 1

A `tokio::signal` future only fires if a tokio reactor polls it, and this
codebase constructs a `tokio::runtime::Runtime` in exactly one place --
`start_spot_server`, reached only when `--server-config` is given. A plain
`manta listen --source foo.wav` runs with no runtime at all, so option 2
would mean either standing up a runtime purely to host a signal task, or
leaving plain `listen` on `ctrlc` while server mode alone moved to
`tokio::signal` -- two signal paths, which is the opposite of what the
ticket asks for. Option 1 sits below that split entirely: it changes only
which signals reach the already-shared `stop` flag.

`termination = []` in `ctrlc`'s manifest carries no dependencies, so
enabling it leaves `Cargo.lock` byte-identical apart from the new `libc`
dev-dependency this ticket's test needs.

## Accepted consequence: SIGHUP now means "shut down"

`ctrlc`'s `termination` feature is all-or-nothing -- it registers SIGINT,
SIGTERM **and** SIGHUP behind the same handler, and `set_handler`'s
closure is a bare `FnMut()` carrying no signal identity, so a subset
cannot be selected through the public API. SIGHUP previously shared
SIGTERM's bug (measured: exit 129 in 13 ms, no drain), so this is a strict
improvement, and `crates/manta-cli/tests/signal_shutdown.rs` pins it
deliberately rather than leaving it incidental.

It does constrain one future design. The 2026-09-05 broad review filed
"live config reload via SIGHUP or `manta reload`" as R-08, built as
MAN-78 (MAN-30 is the unrelated scheduled-segment ticket). SIGHUP is now claimed as a graceful-shutdown
trigger indistinguishable from SIGINT/SIGTERM, so R-08 cannot add reload
behaviour through `ctrlc`'s API. If it is ever built it needs a
signal-distinguishing mechanism of its own (`signal-hook`'s iterator API,
or `tokio::signal::unix::signal(SignalKind::hangup())` on an independent
stream) -- or it should pick the `manta reload` subcommand form instead
and leave SIGHUP alone.

## Related, deliberately not changed here

- **`SHUTDOWN_DRAIN_DEADLINE`'s shape.** MAN-45 records that one flat
  registry-wide budget -- 50 s as MAN-45 left it, sized to outlive
  `2 * telnet::WRITE_TIMEOUT + CLIENT_DRAIN_DEADLINE` = 40 s -- covering
  `await_all`'s entire task registry cannot bound an unbounded number of
  individually-compliant per-client backlogs. That is a pre-existing
  property of the SIGINT path; MAN-85 only makes SIGTERM reach it.
- **`Command::Soak`.** It registers no signal handler at all and stops on
  its own `duration` watchdog (`crates/manta-engine/src/soak.rs`). A soak
  run still cannot be interrupted by any signal.
- **`README.md`'s `docker stop -t 60` guidance.** It is about the drain's
  *duration* exceeding Docker's 10 s default grace period, not about which
  signal triggers it, and stays correct. The 60 s figure is the caller-side
  grace period MAN-45's review round 19 settled on so it stays above the
  50 s `SHUTDOWN_DRAIN_DEADLINE`; the `Dockerfile` comment carries the same
  pair. Both must move together if that deadline ever grows again.

## Migration note: MAN-75 packaging artifacts become redundant, not wrong

MAN-75 (in flight at the time this ticket was implemented) ships
`packaging/systemd/manta.service` with `KillSignal=SIGINT`, a launchd
`kill -INT` workaround in `packaging/README.md`, and a `docker-compose.yml`
comment noting the image's (now-removed) `STOPSIGNAL SIGINT`. None of those
break after this change -- SIGINT still drains identically -- but they
become unnecessary. Follow-up cleanup, once MAN-75 is on `main`:

- `packaging/systemd/manta.service` -- drop `KillSignal=SIGINT` and its
  comment, and the matching `assert_eq!(get("Service", "KillSignal"),
  "SIGINT")` test.
- `packaging/README.md` -- drop the launchd `kill -INT` workaround.
- `docker-compose.yml` -- update the stale "the image already sets
  `STOPSIGNAL SIGINT`" comment; the `stop_signal`-absent assertion itself
  stays correct.

## Superseded in part by MAN-78 (2026-10-10)

MAN-78 took the `signal-hook` route this record named above. The workspace
now declares plain `ctrlc = "3"`: ctrlc keeps SIGINT (and Ctrl-C/Ctrl-Break
on Windows, where `termination` never applied). On Unix, `signal-hook`
registers SIGTERM on the same `stop` flag, and SIGHUP either on that flag
(no `[server]` table: drain and exit 0, as before) or on a
`signal_hook::iterator::Signals` handle that the `manta-reload` thread
reads (with `[server]`: reload the `[spot]` lists and `[[rbn_uplink]]`
`dry_run`). All of them are installed at the same point as before, ahead
of the `listening:` banner. The SIGINT/SIGTERM drain, the exit code and
`signal_shutdown.rs`'s five tests are unchanged. See
`docs/DECISIONS/2026-10-10-man78-live-reload.md`.
