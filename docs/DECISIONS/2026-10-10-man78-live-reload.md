# 2026-10-10 — MAN-78: live reload of the `[spot]` lists and uplink `dry_run`

**Status:** Implemented (branch `MAN-78`). Records how a running daemon
picks up an edited blocklist, notch list, allowlist or `[[rbn_uplink]]`
`dry_run` without a restart. Origin: the 2026-09-05 broad review, R-08 /
lens 2 #25. Partly supersedes
`docs/DECISIONS/2026-09-07-man85-signal-handling.md` (SIGHUP no longer
always means "shut down"), and builds on MAN-261's single config file and
MAN-76's `manta config check`.

## Context

Before this change the only way to apply an edited list file or `dry_run`
was a restart. A restart drops every telnet, JSON and WebSocket client,
loses the `sh/dx` history and reconnects the RBN uplink. Legacy precedent
(Aggregator's Bad Calls / Notched Frequencies files, SkimSrv's `.ini`) takes
such edits without a restart.

Reproduced at `13131ca`: a `[server]` daemon with a logged-in telnet client
and a `[spot] blocklist_path`, sent SIGHUP, closed the client socket 9 ms
later and exited 0 11 ms after that, with no reload and no notice. SIGHUP
was a full shutdown, because ctrlc's `termination` feature registered
SIGINT, SIGTERM and SIGHUP behind one signal-agnostic closure (MAN-85).

## Who sees what

| Signal | `manta run` **with** `[server]` (Linux, macOS) | `manta run` **without** `[server]` | Windows |
|---|---|---|---|
| SIGINT / Ctrl-C | drain, exit 0 (unchanged) | drain, exit 0 (unchanged) | drain, exit 0 (unchanged) |
| SIGTERM | drain, exit 0 (unchanged) | drain, exit 0 (unchanged) | n/a |
| SIGHUP | **reload**: process, clients, `sh/dx` history and uplink connections stay | drain, exit 0 (unchanged) | n/a (no SIGHUP, no reload) |

A daemon that reloads says so in its readiness line:
`manta: listening; send SIGINT or SIGTERM to stop, SIGHUP to reload [spot] lists and dry_run`.

## What a reload applies

- `[spot] allowlist`, and the files `[spot] blocklist_path` and
  `[spot] notch_path` name (or `--allowlist`/`--blocklist`/`--notch`, which
  keep overriding the file).
- Each `[[rbn_uplink]]` entry's `dry_run`, by position.

Every other key is restart-only. A reload that finds one changed applies
the lists anyway and names it in a warning. This includes `spot.cty_path`,
`spot.scp_path` and `rbn_uplink[N].spot_types`. The country and SCP files
are re-read and validated, but the running tables are kept. Replacing their
contents at the same path also needs a restart.

## Decisions

- **D1. SIGHUP is the only trigger.** It is the first trigger the ticket
  names and the Unix daemon convention (systemd
  `ExecReload=/bin/kill -HUP $MAINPID`, `docker kill --signal=HUP`). The
  kernel's same-UID-or-root rule already decides who may send it. A
  `manta reload` verb would need a way to find the daemon, and none exists:
  a PID file adds a config key, stale-file handling and a wrong-process risk
  across PID namespaces; a `POST /reload` would turn the unauthenticated,
  read-only metrics listener (MAN-132) into a control surface.
- **D2. SIGHUP reloads only in daemon mode (`[server]` present).** That is
  the predicate that starts the servers (MAN-261 D7), and the only case with
  clients, history and an uplink to protect. Without `[server]`, SIGHUP
  keeps MAN-85's drain-and-exit meaning, so a terminal hangup still ends a
  foreground run.
- **D3. Signal mechanism.** ctrlc's `termination` feature is dropped. ctrlc
  keeps SIGINT on Unix and Ctrl-C/Ctrl-Break on Windows. On Unix only,
  `signal-hook` (`default-features = false, features = ["iterator"]`)
  registers SIGTERM on the existing `stop` flag, and SIGHUP either on a
  `Signals` iterator (daemon mode) or on the `stop` flag. ctrlc refuses to
  share a signal (`MultipleHandlers`, `ctrlc-3.5.2/src/platform/unix/mod.rs`),
  so it must not own SIGTERM or SIGHUP any more. Everything is installed at
  the old handler site, before the `listening:` banner, so MAN-122's
  covered-window guarantee holds for every signal; the `Signals` handle
  buffers a SIGHUP that arrives before the reload thread starts.
  `Cargo.lock` gains `signal-hook 0.4.5` and `signal-hook-registry 1.4.8`.
  In daemon mode `main` also blocks SIGHUP (`pthread_sigmask`) on the decode
  thread before it opens the source. Every later thread inherits the mask,
  and only the reload thread unblocks it. Without the mask, Linux ran the
  handler on the decode thread, which failed an in-flight KiwiSDR/HPSDR
  socket `recv` with EINTR (a read timeout makes it non-restartable even
  under `SA_RESTART`), and the source dropped and reconnected (validate
  round 1; `config_reload.rs` `sighup_does_not_drop_a_kiwisdr_source`).
  SIGINT and SIGTERM still reach the decode thread.
- **D4. Reload = `run`'s config stage, all or nothing.** The reload thread
  re-runs `prepare_config` (the non-printing core of `prepare_live`: load,
  `MANTA_*` overlay, CLI merge, blocklist/notch reads) with the startup
  `CliOverrides`, `--engine`, config path and the startup environment,
  captured in `ReloadContext.vars`, then the checks `config check` adds
  on top of that stage: the `<...>` placeholder and `N0CALL`
  scan, and duplicate listener ports (PR #232 review). When the command
  line names the source, `[input]` is left out of the placeholder scan,
  because that flag replaced it (MAN-268 D3). `run` checks only the
  callsigns, so a daemon started from a file with another placeholder or a
  port clash has every reload rejected, naming the key, until that file is
  fixed; under the systemd unit (D9) that takes a restart. Any error
  rejects the reload before anything is applied, so `manta config check` passing means the
  reload passes (barring an edit between the two), and command-line flags
  keep winning. Malformed list lines are still skipped silently, exactly as
  at startup (MAN-76 D3). `prepare_live` prints the bundled-cty age warning
  at startup only; the non-printing `prepare_config` never prints it on
  SIGHUP.
- **D5. Apply points.**
  - **Lists** go into `manta_engine::OperatorListsUpdate`, a newest-offer-
    wins slot. `emit()` takes it before each decoder event reaches
    `Validator::ingest`, and `Validator::replace_operator_lists` swaps all
    three lists as one unit. This is the only apply point, shared by
    calibration, steady-state reads, outages and the end-of-stream flush.
    Tracks, dedupe, the repetition gate, the ledger and the suppression counters are untouched; a word already attempted under
    the old lists is not re-evaluated. `resolve_pending_beacons` now
    re-checks the blocklist and notch list before `gate.record`, so a beacon
    captured before a reload cannot escape it at track close.
  - **`dry_run`** is one `Arc<AtomicBool>` per target, read per forwarded
    spot after MAN-91's `spot_types` filter by `uplink::serve_with_live_dry_run`
    (`uplink::serve` keeps its signature and seeds the flag from `config.dry_run`). It is applied by
    position, and only when no `[[rbn_uplink]]` entry changed in any other
    way; otherwise the reload logs that no `dry_run` change was applied.
    A target with `enabled = false` runs no task, so its `dry_run` change
    is not logged.
- **D6. Restart-only changes warn; they do not reject.** The reload diffs
  the startup `Loaded.raw` against the new one: tables recurse (a missing
  table counts as empty), arrays recurse per index when the lengths match
  and are reported whole otherwise, other values compare by equality.
  Only `spot.allowlist`, `spot.blocklist_path`, `spot.notch_path` and
  `rbn_uplink[N].dry_run` are excluded. Every other changed leaf is named,
  including `spot.cty_path`, `spot.scp_path` and `rbn_uplink[N].spot_types`.
  Any other uplink edit, including an entry-count change, holds back every
  `dry_run` change in that reload. Lists still apply, so an unrelated edit
  cannot block an urgent blocklist update.
- **D7. The reload thread is synchronous.** A named std thread
  (`manta-reload`) iterates `Signals::forever()`, does the file I/O off the
  decode thread and logs through `tracing` (it starts after
  `start_spot_server`, so the subscriber exists). After file I/O,
  `reload_once` locks `DrainGate` and checks both the gate and `stop` before
  applying anything. It holds that lock through list publication and the
  flag swaps. When `listen_with_observers` returns, `main` closes the gate
  under the same lock and sets `stop`. A reload that loses this race returns
  `Ok(ReloadOutcome::ShuttingDown)` and changes nothing. Both this case and
  a SIGHUP received after shutdown began log the same INFO line:
  `reload: SIGHUP ignored; the daemon is shutting down`. Invalid configs
  still log at ERROR.
- **D8. Determinism.** Without a SIGHUP the decode path does one extra
  relaxed atomic load per decoder event. The pending-beacon re-check is a
  no-op, so replay output stays byte-identical (the golden tests are unchanged). A
  reload during a file replay makes the output depend on when the signal
  arrived, by design. `listen()`, `soak`, `doctor` and the benches pass no
  handle and pay nothing.
- **D9. Packaging.** MAN-268's kit landed on `main` before this change, so
  `packaging/systemd/manta.service` gains `ExecReload=/bin/kill -HUP $MAINPID`
  (pinned by `packaging_examples.rs`). Its `LoadCredential=` copies the
  config only at service start, so `systemctl reload manta` re-reads that
  start-time copy: list-file edits (absolute paths) apply, an edit to
  `/etc/manta/manta.toml` itself still needs a restart; the unit comment and
  `packaging/README.md` say so. The macOS copy-truncate rotation rationale
  ("SIGHUP shuts manta down") is corrected in `packaging/README.md`,
  `packaging/launchd/rotate-log.sh` and the MAN-268 decision record; the
  conclusion stands, because a reload reopens no log either.
- **D10. Preserve the existing implementation.** Carry forward branch
  `MAN-78` and integrate current main, including MAN-79 and MAN-91. Keep
  the earlier fixes for signal delivery, config-check parity, disabled
  uplink logging and the post-I/O stop check.
- **D11. Review convergence.** PR #232 continues its existing review round
  count. Fix correctness and contract findings inline; record later-round
  P2-and-lower findings in its follow-up ticket, per the repository policy.

## Re-plan (2026-10-10, MAN-296)

The original chunk-level apply point missed reloads during calibration,
an in-flight read and EOF. Two remediation reports described a calibration
fix, but `git grep apply_pending_lists origin/MAN-78` at `3af7553` found
none. This re-plan makes `emit()` the sole apply point and pins all three
windows with non-vacuous tests. It also integrates MAN-79's cty/scp keys
and MAN-91's uplink filter, so restart-only changes are named, and makes
shutdown-raced reloads an INFO outcome.

## Log lines

```text
INFO  manta::reload: reload: SIGHUP received, re-reading /etc/manta/manta.toml
INFO  manta::reload: reload: applied allowlist_calls=1 blocklist_calls=2 blocklist_path=/etc/manta/bl.txt notch_ranges=0 notch_path=none
WARN  manta::reload: reload: dry_run = false -- transmitting real spots to this target. target=rbn.example.org:7000
INFO  manta::reload: reload: dry_run = true -- connected, but NOT transmitting spots to this target. target=rbn.example.org:7000
WARN  manta::reload: reload: these settings changed but take effect only after a restart; keeping the running values: decode.engine, server.telnet_port
WARN  manta::reload: reload: [[rbn_uplink]] changed beyond dry_run; no dry_run change applied until a restart
ERROR manta::reload: reload: rejected; still running the previous configuration: reading blocklist file /etc/manta/bl.txt: No such file or directory (os error 2)
INFO  manta::reload: reload: SIGHUP ignored; the daemon is shutting down
```

The counts are distinct callsigns and parsed ranges, so a list file emptied
by mistake shows as `blocklist_calls=0`. `dry_run` levels follow MAN-159:
`warn` when real spots start going out, `info` for the safe state.

## Evidence

- `crates/manta-cli/tests/config_reload.rs` drives both Gherkin scenarios
  against a real daemon: a blocklist edit + SIGHUP lets a previously blocked
  W1AW reach the logged-in telnet client, a second reload names a changed
  `server.operator_qth` and `spot.cty_path`, and `sh/dx` on the same socket replays W1AW; a
  UTF-16 blocklist and a broken `[spot` header are each rejected with the
  error chain while W1AW stays blocked, and a later valid reload still
  applies (lists and `dry_run`).
- `signal_shutdown.rs`'s five tests pass unchanged (SIGTERM, SIGINT,
  SIGHUP without `[server]`, Dockerfile, signal at the banner).
- Unit tests: `validator.rs` (list replacement, pending-beacon re-check),
  `listen.rs` (mid-run application on vector v7: N2BB spots at sample
  1,407,232, N1AA at 1,555,968, so lists offered at N2BB decide N1AA),
  `reload.rs` (`reload_once`, `restart_only_changes`) and
  `uplink_acceptance.rs` (a flipped flag applies to the next spot on the
  same connection, no reconnect).
- The earlier planning-session prototype reproduced the two Gherkin
  scenarios against a live daemon. The re-plan repeated it on the merged
  branch with two valid reloads and three rejected ones: a missing cty
  file, a UTF-16 blocklist and malformed TOML. The client stayed connected,
  `sh/dx` answered and SIGTERM exited 0 (plan E9).
- Re-plan E3 merged main into `3af7553` with three conflict hunks in
  `uplink.rs`. Keeping MAN-91's filter before the live flag passed all 18
  uplink tests and the full workspace suite in that planning session.
- E4 measured W1AW emitted at reads 1 (calibration, 72 WPM), 16 (steady
  state, 60 WPM) and 9 (EOF after 205,000 samples). A blocklist offered
  inside those reads leaked W1AW with the old apply point. With `emit()`
  taking the lists, all three runs emitted no spots. Implementation tests
  `lists_offered_during_*` reproduce the failures before the fix and pass
  after it, each asserting the control run first.
- E6 found that `spot.*` hid cty/scp edits from restart warnings. The new
  `restart_only_changes_reports_*` tests pin both paths, a newly appearing
  `[spot]` table and the uplink's `spot_types` key.

- The implementation run repeated E9 on the new binary: two valid reloads,
  three rejected reloads, a named `spot.cty_path` restart warning, history
  replay on the original telnet socket and SIGTERM exit 0. The three CLI
  reload integration tests and all 20 reload unit tests also passed.

## Not done here

- No `manta reload` subcommand, PID file, control socket or `POST /reload`.
- No reload of anything except the three operator lists and uplink `dry_run`; no
  reload on Windows, and none for `soak` or `doctor`.
- No automatic reload on file change (no inotify): half-saved editor files
  would cause spurious rejections.
- No withdrawal of already-published spots; a newly blocklisted call stays
  in `sh/dx` history and in anything already sent upstream.
- ARCHITECTURE.md §10's concurrency diagram still shows the validator off
  the main thread; it actually runs on the main thread. That predates this
  ticket.

## Follow-ups

1. **MAN-96 field node:** decide whether a reload counts as a planned
   change or a manual intervention in the 30-day ledger, and add
   `ExecReload=/bin/kill -HUP $MAINPID` to `manta-field.service`.
2. **`manta reload` verb**, if operators ask for one (D1's reasons).
3. **Strict list-file parsing**: report malformed lines instead of
   skipping them, at startup and on reload alike.
4. **Reload metrics**: a `manta_config_reloads_total{result}` counter and a
   `/status` field for the last reload.

5. Country/SCP table hot reload across validation and geography consumers.
6. Uplink `spot_types` hot reload.
