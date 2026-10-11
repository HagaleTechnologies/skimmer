# 2026-10-10 — MAN-268: unattended service kit and the example-callsign startup guard

**Status:** Implemented (branch `MAN-268`). Ships an example config and
service files for systemd, macOS launchd and Docker Compose, in the
repository and in every release archive, and makes `run` refuse the
example callsign as the station or an uplink login. Supersedes MAN-75
(PR #124, closed unmerged on 2026-10-06) and resolves its six open review
findings. Native installs on real systemd, macOS and Docker hosts have not been run (see
Consequences). Operator instructions: `packaging/README.md`.

## Context

- Nothing on `main` let an operator run manta unattended without writing
  the config and the service files by hand. Release archives held the
  binary, `README.md` and the two licenses.
- MAN-76's `manta config init` already writes every key commented out at
  its default, and its `config check` rejects unedited placeholders. `run`
  did not. Reproduced on `0cd6a30`: a config with
  `station_callsign = "N0CALL"`, a local 48 kHz source and
  `--dial-freq-hz` started with `station=N0CALL`, opened all three
  listeners and exited 0 on SIGTERM.
- MAN-85 made SIGTERM drain like SIGINT. A clean stop is bounded by
  `SHUTDOWN_DRAIN_DEADLINE` (50 s, `crates/manta-cli/src/main.rs`) plus a
  2 s `Runtime::shutdown_timeout`: 52 s worst case. The ticket's technical
  notes carried MAN-75's `TimeoutStopSec=30` and `stop_grace_period: 30s`,
  sized when the deadline was 25 s.
- MAN-96 shipped a field-specific systemd unit (static `manta` user,
  `TimeoutStopSec=60`) and said its runbook would link general
  `packaging/` units as the general-purpose alternative once they landed.

## Decisions

- **D1 — kit contents.** Root `manta.example.toml` and
  `docker-compose.yml`; `packaging/README.md`, the operator guide;
  `packaging/systemd/manta.service`; and
  `packaging/launchd/com.hagaletechnologies.manta.plist`,
  `com.hagaletechnologies.manta-logrotate.plist`,
  `create-service-account.sh` and `rotate-log.sh`. Release archives carry
  them at the same relative paths, plus
  `docs/RUNBOOKS/network-exposure.md`, which the guide links. The guide
  links no other repository file, so its relative links resolve in both
  the repository and an extracted archive.
- **D2 — the example is the scaffold plus documented edits.**
  `manta.example.toml` is `manta config init --out -` with a header naming
  the file, an active `[server]`, and an active
  `station_callsign = "N0CALL"` below the commented reference line. Every
  other key stays commented at its default; no host, receiver or RF
  frequency is active. `config init`'s own output is unchanged.
  `packaging_examples.rs` applies the edits to the live scaffold and
  compares the result with the file, so a change to `config_init.toml`
  fails the build until the example follows.
- **D3 — callsign-only startup guard.** The exact `N0CALL` comparisons
  (case-insensitive) for `server.station_callsign` and
  `rbn_uplink.login_callsign`, and their diagnostics, until now inside
  `config check`, are the crate-visible
  `config_cmd::reject_example_callsigns`, which both `config check` and the
  `Command::Run` arm call. `run`, and its alias `listen`, call it right
  after `prepare_live` returns: before `is_rf_aware()` (which can open IQ
  WAVs), the dial-frequency guard and any source or listener I/O. It reads
  the typed config after the `MANTA_*` overlay, so
  `MANTA_SERVER_STATION_CALLSIGN` can supply a real call over the file's
  placeholder, or reintroduce `N0CALL` over a real one. The station failure
  is
  `Error: <config>: server.station_callsign is still the example "N0CALL" -- set your own callsign`,
  exit 1, empty stdout. Callsign grammar and SSID handling are unchanged.
  The broad `<...>` scan stays check-only: a CLI source flag replaces the
  whole `[input]` table (MAN-261), so a placeholder left in an unused
  `[input]` is legitimate for `run`, and scanning raw values would refuse
  valid invocations. The uplink login is checked at startup too (added in
  self-review; the plan had the station only): the example tells an
  operator to uncomment the whole `[[rbn_uplink]]` block, including
  `login_callsign = "N0CALL"`, no flag replaces that table, and with
  `dry_run = false` the daemon would send spots to a collector under the
  placeholder login. `decode`, `oracle`, `soak`, `doctor` and `status` get
  no new refusal.
- **D4 — 60-second stop budgets, not the ticket's 30.**
  `TimeoutStopSec=60`, launchd `ExitTimeOut` 60 and Compose
  `stop_grace_period: 60s`. A 30 s budget would SIGKILL a manta that is
  still draining and silently truncate the client backlogs MAN-45's drain
  exists to deliver or record. 60 s is the 52 s worst case plus margin,
  the value MAN-96's field unit, the Dockerfile comment and README's
  `docker stop -t 60` already use. `packaging_examples.rs` reads the drain
  deadline and runtime cutoff from `main.rs` and requires every budget to
  exceed their sum by at least 5 s, so raising the deadline fails the
  build until all three files follow. No file overrides the stop signal
  (`KillSignal`, `stop_signal`, `kill -INT`): every manager's default
  SIGTERM drains, so MAN-85's migration note for PR #124 has nothing left
  to clean up.
- **D5 — systemd: `DynamicUser=yes` with a credential.** The unit keeps
  the ticket's `DynamicUser=yes`. The config reaches the transient user
  through `LoadCredential=manta.toml:/etc/manta/manta.toml`: systemd, as
  root, copies the root-owned mode-0600 file into the service's private
  credentials directory, and `ExecStart` passes
  `--config ${CREDENTIALS_DIRECTORY}/manta.toml`, which systemd expands in
  `ExecStart` from v247, the minimum the guide states. A KiwiSDR password
  never sits in a world-readable file, and nothing relies on
  `ConfigurationDirectory=` changing the ownership of files inside it. The
  guide states what follows: manta resolves relative paths against the
  credentials directory, so file input, blocklist and notch paths must be
  absolute, and outside `/tmp`, which `DynamicUser` makes private; the
  transient user has no device groups, so the recipe uses a network
  receiver; an edited config takes effect at the next start.
  `Restart=always`, `RestartSec=10`, `StartLimitIntervalSec=0` and journal
  output match MAN-96's unit. Journal retention is the host's
  `SystemMaxUse=`/`RuntimeMaxUse=`; the kit installs no journald override.
  MAN-96's static-user field unit is unchanged, and its runbook now links
  this kit as the general alternative.
- **D6 — macOS: one LaunchDaemon, account first.** A boot-time
  LaunchDaemon runs `/usr/local/bin/manta run --config
  /usr/local/etc/manta/manta.toml` directly, with no shell between launchd
  and manta, as `_manta`, with `KeepAlive` true and `ThrottleInterval` 10.
  No LaunchAgent variant: one install path, without mixing user-home paths
  into root's launchd domain. `create-service-account.sh` comes first in
  the guide's commands, before either plist is installed or bootstrapped.
  It creates a disabled-login `_manta` user and group with `dscl`, using
  an ID from 400–499 that is free in the local records and through
  directory resolution (no fixed ID), shell `/usr/bin/false`, home
  `/var/empty` and no password, verifies them with `id _manta`, and prints
  `manta service account ready: _manta`. A re-run accepts a matching
  account. An incompatible or partial `_manta`, an exhausted range or a
  failed command exits non-zero on stderr without deleting or overwriting
  anything. The config is `root:_manta` mode 0640 in a 0750 directory.
- **D7 — macOS log retention: periodic copy-truncate.** launchd sends
  stdout and stderr to `/var/log/manta/manta.log`. A companion job
  (`com.hagaletechnologies.manta-logrotate`: `RunAtLoad`, `StartInterval`
  60, `KeepAlive` false, as `_manta`, its own output to `/dev/null`) runs
  `rotate-log.sh`. When the log is a regular, non-symlink file over
  10 MiB, the script copies its final 1 MiB to a unique temporary file in
  the same directory, overwrites the log in place and removes the
  temporary file; any failure exits non-zero, which `launchctl print`
  shows as the job's last exit code. Not newsyslog: rename rotation needs
  the writer to reopen its file, manta never reopens the stdout and
  stderr it inherits, and SIGHUP, newsyslog's default signal, shuts manta
  down (MAN-85; since MAN-78 a `[server]` daemon reloads its `[spot]`
  lists on SIGHUP instead, which reopens no log either). manta would keep writing to the renamed file, so the open
  inode has to stay. Accepted limits, stated in the guide: the log can
  exceed 10 MiB between runs or while the job cannot run, and lines
  written during the copy can be lost. This is operational retention, not
  a strict disk quota or a lossless audit log. The rotator keeps no log
  of its own that could grow without bound.
- **D8 — Docker Compose.** Root `docker-compose.yml`:
  `restart: unless-stopped`; `command: ["run", "--config",
  "/etc/manta/manta.toml"]`; `stop_grace_period: 60s` with Docker's
  default SIGTERM; `./manta.toml` bound read-only as a single file with
  `create_host_path: false`, so a missing config fails the start instead
  of becoming an empty directory, and `selinux: Z`, which relabels that
  host file with a private label for this container on SELinux hosts
  (host-side, hence a dedicated file and never a shared directory); telnet
  7300 and JSON 7301 published on every interface; metrics bound to
  `0.0.0.0` inside the container through `MANTA_SERVER_METRICS_BIND_ADDR`
  and published only on the host's `127.0.0.1:7302` (MAN-132);
  `RUST_LOG=info`; `json-file` logging with `max-size: "10m"` and
  `max-file: "3"`. No device passthrough, privileged mode or host
  networking. The image is `ghcr.io/hagaletechnologies/manta:latest`,
  which has not been published, so the guide documents
  `docker build -t manta:local .` in a source checkout (an archive has no
  Dockerfile) and changing `image:`. The Dockerfile's `useradd --system`
  user has no fixed UID, so the guide shows a mode-0644 config for a
  public receiver, and a 0640 config in the container user's group before
  a password is stored.
- **D9 — one archive builder.** `scripts/package-release.py` (Python
  standard library only) stages the binary, README, licenses and the D1
  files in a fresh directory, writes the tar.gz or zip with today's
  archive names, top-level directory and binary mode, and fails without
  writing an archive when an input is missing. `release.yml` and
  `release-publish.yml` call it from one `shell: bash` step on every
  runner, replacing the four inline packaging steps. Build matrices,
  static-CRT checks, triggers, permissions, action pins and MAN-244's
  ancestry and approval gates are unchanged.
- **D10 — tests.**
  - `crates/manta-cli/tests/packaging_examples.rs`: the example against
    the scaffold; `config check` and `run`/`listen` refusing the unedited
    example in any case, before the dial-frequency guard and before the
    source opens; a copied example with a real call, a generated 48 kHz
    source and `--dial-freq-hz` listening and exiting 0 on SIGTERM; parsed
    systemd and Compose directives and the stop-budget arithmetic.
  - `crates/manta-cli/tests/config_command.rs`: the guard after the
    environment overlay, valid portable and SSID calls, and a source flag
    replacing a placeholder `[input]`.
  - `scripts/tests/test_packaging.py`: both plists parsed with `plistlib`;
    `rotate-log.sh` run on real files (threshold, retained suffix, same
    inode with a writer appending across rotation, unsafe paths, failures,
    cleanup); `create-service-account.sh` against `dscl`/`dscacheutil`/`id`
    stubs (fresh host, matching account, occupied IDs, incompatible and
    partial identities, exhausted range, failed commands).
  - `scripts/tests/test_package_release.py`: builds both archive formats,
    extracts them and checks paths and bytes, missing inputs and stale
    output, and that both workflows invoke the helper in executable step
    bodies rather than comments.

  CI runs both Python suites in the required `test` job: the packaging
  tests on the Unix legs, the archive tests on all three.

## PR #124's open findings

| # | Finding | Resolution |
|---|---|---|
| 1 | P1: reject the placeholder station callsign at startup | D3: `run`/`listen` refuse `N0CALL` before any source or listener I/O |
| 2 | P1: create the account before assigning `UserName` | D6: `create-service-account.sh` is the first macOS command, before any plist is installed or bootstrapped |
| 3 | P2: rotate the LaunchAgent's growing log | D6/D7: one LaunchDaemon plus a once-a-minute copy-truncate job, with its limits documented |
| 4 | P2: bounded logging for the Compose service | D8: `json-file` with `max-size: "10m"`, `max-file: "3"` |
| 5 | P2: verify copy commands instead of matching comments (`release.yml`) | D9/D10: one helper builds every archive; tests build and inspect real archives and look for the invocation only in executable step bodies |
| 6 | P2: the same, for `release-publish.yml` | As 5 |

## Consequences

- An operator who copies the example, sets a callsign and a receiver, and
  runs `config check` can install any of the three services from an
  extracted archive.
- `run` and `listen` now refuse a station callsign or uplink login of
  exactly `N0CALL`; anyone deliberately running under it must use their
  own call. Config precedence is unchanged, `run` still discovers no config file, and no
  existing install or operator file is changed.
- The three stop budgets and `SHUTDOWN_DRAIN_DEADLINE` move together; the
  test enforces it.
- The tests parse and execute the artifacts but cannot replace native
  installs, which need hosts CI does not have: systemd 247 or newer
  (credential readability, a stop through the drain with clients
  attached, journal retention), macOS (account creation on a fresh host,
  `plutil`, bootstrap, rotation while manta writes, `bootout`) and Docker
  (rendering, the missing-file refusal, loopback-only metrics, log limits,
  SELinux relabeling on an enforcing host). None of these has been run.

## Out of scope

Package managers (deb, rpm, Homebrew), code signing and notarization, a
Raspberry Pi image, a Windows service wrapper, publishing the container
image, a macOS LaunchAgent, a streaming log supervisor or strict log
quota, and field-node deployment, RBN approval and live hardware
acceptance.

## References

- Ticket: MAN-268, superseding MAN-75 (PR #124; tag
  `archive/MAN-75-pr124` at `3568e1d8a5`, read as prior art only)
- Research and plan: the thoughts pool's
  `2026-10-10-MAN-268-operators-should-get-an-example-config-and-service-unit`
  documents
- `packaging/README.md` (operator guide)
- `docs/DECISIONS/2026-10-07-man76-config-check-init.md` (scaffold and
  `check`; its placeholder follow-up is annotated)
- `docs/DECISIONS/2026-09-07-man85-signal-handling.md` (SIGTERM drain,
  migration note)
- `docs/DECISIONS/2026-10-08-man132-metrics-loopback-bind.md` (metrics
  bind)
- `docs/DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md`
  (field unit and scope boundary)
- `docs/DECISIONS/2026-10-10-man244-release-governance.md` (release
  gates, unchanged)
- [systemd credentials](https://systemd.io/CREDENTIALS/),
  [systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html),
  [launchd.plist(5)](https://raw.githubusercontent.com/apple-oss-distributions/launchd/main/man/launchd.plist.5),
  [newsyslog.conf(5)](https://raw.githubusercontent.com/apple-oss-distributions/syslog/main/newsyslog/newsyslog.conf.5),
  [Compose services](https://docs.docker.com/reference/compose-file/services/),
  [json-file logging](https://docs.docker.com/engine/logging/drivers/json-file/),
  [bind mounts](https://docs.docker.com/engine/storage/bind-mounts/)
