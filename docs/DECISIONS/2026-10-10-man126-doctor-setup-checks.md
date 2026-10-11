# MAN-126: `manta doctor` checks the whole setup and names each failing check

An operator should be able to run one command that says whether their manta
setup is healthy and, when it is not, which check failed and what to do.
Before this change, `manta doctor` answered only "is the receiver hearing
anything?". It ignored `[server]` with a note, never read `[[rbn_uplink]]`,
and reported a receiver that would not open as a generic `Error:` chain.

`manta doctor` now runs named setup checks before its existing signal check.
The operator runbook is [setup-checks](../RUNBOOKS/setup-checks.md). The
code is `crates/manta-cli/src/doctor_checks.rs` (model, renderer, summary,
and the config, audio, port, uplink, receiver and signal checks) and
`crates/manta-cli/src/clock_check.rs` (SNTP client, Linux kernel sync
query, clock classifier). `manta_engine::doctor()`, `DoctorReport`,
`Verdict` and the signal report text are unchanged.

## Decisions

- **D1 — One command.** The ticket names `manta doctor`. Setup checks run in
  front of the existing signal check; there is no new subcommand.
- **D2 — Statuses and exit.** Each check is PASS, WARN, FAIL or SKIP. WARN
  and FAIL always carry a fix, and only their constructors take one. Doctor
  exits 1 if and only if a check failed (or doctor could not start). The
  signal verdict, including `NO_SIGNAL`, stays out of the exit code, as
  before: a quiet band is not a broken setup.
- **D3 — The config check is `run`'s refusal list.** Only the two refusals
  `run` makes before any I/O: the example `N0CALL` callsign
  (`config_cmd::example_callsign_key`) and the dial-frequency guard
  (`needs_dial_freq`). Both are shared with `run`, so the two cannot drift,
  and `run`'s messages are byte-identical. Load and validation errors stay
  hard errors with their existing text; `manta config check` covers the
  rest.
- **D4 — Ports.** Each `[server]` listener is bound with `run`'s
  address/port shape and released at once (`std::net::TcpListener` sets
  `SO_REUSEADDR` on Unix and not on Windows, as tokio does, so the probe
  fails when `run`'s bind would). Two listeners on one port fail without
  binding, by `config check`'s overlap rule (`config_cmd::duplicate_of`).
  Port 0 is a SKIP. The probe is advisory: another program can take the
  port between doctor and `run`.
- **D5 — Clock.** One SNTP v4 request measures the offset that matters
  (spot times) on every platform. On Linux, `adjtimex` with `modes = 0`
  also reads the kernel's `STA_UNSYNC`/`TIME_ERROR` state: read-only,
  unprivileged, and the same answer whether chrony, systemd-timesyncd or
  ntpd runs, with no subprocess. In a container it reads the host kernel,
  whose clock the container uses. WARN at 1 s off (far outside any working
  NTP client's error); FAIL at 60 s (the RBN spot line's one-minute time
  resolution). The default server is `pool.ntp.org`, overridable with
  `--ntp-server`; there is no switch to turn the check off, and on Linux the
  kernel state still gives a PASS when the server cannot be reached.
- **D6 — Uplink.** Each enabled target is resolved, then each address is
  tried with a 5 s connect timeout, all within 10 s per target. The first
  accepted connection is dropped at once. Doctor reads no prompt and sends
  no login: a collector's prompt may lack a newline, and the login exchange
  is `manta status`'s territory once the daemon runs. Rows use the daemon's
  metrics labels (`host:port`, `#N` for repeats).
- **D7 — Audio.** Sound-card inputs are enumerated only when the receiver is
  a sound card; listing them for a KiwiSDR node would be noise. The Linux
  "ALSA runtime library is installed" clause is true by construction: the
  process could not have started without `libasound.so.2`.
- **D8 — Named receiver and signal failures.** A receiver that will not open,
  and a decode run that errors after it opened, are FAIL lines with a fix
  per receiver kind, not an `Error:` exit. Doctor flushes stdout and exits 1
  itself, as `manta status` does, so no generic error line follows.
- **D9 — JSON.** `--json` keeps every existing key and adds `checks` and
  `checks_status`. Without a signal report (the receiver did not open, or
  the signal check stopped) the object holds only those two.
- **D10 — Supersedes MAN-261 D7 for doctor.** Doctor now reads `[server]`
  and `[[rbn_uplink]]` to check them, still never starting a server or
  logging in to a collector, and no longer prints the "ignoring [server]"
  note. `soak` is unchanged.

Network checks (clock and every enabled uplink) run at the same time, each
on its own thread; doctor waits for each until its budget plus 1 s and then
abandons it, because a stalled name lookup cannot be cancelled. Typical cost
is one round trip each; the worst case adds 10 s.

## Not done, and follow-ups

- **A missing `libasound2` cannot be detected from inside manta.** ALSA is
  linked at build time (`ldd manta` lists `libasound.so.2`), so the loader
  refuses to start the binary and no Rust code runs. The loader's own
  message names the library; the runbook and `packaging/README.md` map it to
  its fix. Follow-up: load ALSA at run time, or make the sound-card backend
  optional for non-audio builds, so a KiwiSDR/HPSDR node runs without
  `libasound2` and doctor can report it.
- No macOS or Windows kernel time-sync query; the SNTP offset covers them.
- No SoapySDR device enumeration in the audio check; the receiver line
  covers Soapy failures.
- No `--setup-only` flag, no config keys for doctor, no `MANTA_*` variable
  for `--ntp-server`.
- No frequency-calibration check (MAN-127) and no device-listing command
  (MAN-125).
