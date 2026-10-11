# Running manta unattended

This kit runs manta as a service that starts at boot, starts again when it
exits, and stops cleanly: a systemd unit for Linux, a LaunchDaemon for
macOS, and a Docker Compose file. It ships in every release archive. Paths
in this guide are relative to the top-level directory of the extracted
archive (in a source checkout, the repository root); run every command from
there.

| File | Purpose |
|---|---|
| [`manta.example.toml`](../manta.example.toml) | Example config: the station callsign is active, every other setting is commented out at its default |
| [`docker-compose.yml`](../docker-compose.yml) | Docker Compose service |
| [`packaging/systemd/manta.service`](systemd/manta.service) | systemd unit |
| [`packaging/launchd/com.hagaletechnologies.manta.plist`](launchd/com.hagaletechnologies.manta.plist) | macOS LaunchDaemon |
| [`packaging/launchd/com.hagaletechnologies.manta-logrotate.plist`](launchd/com.hagaletechnologies.manta-logrotate.plist) | macOS log rotation job |
| [`packaging/launchd/create-service-account.sh`](launchd/create-service-account.sh) | Creates the macOS `_manta` service account |
| [`packaging/launchd/rotate-log.sh`](launchd/rotate-log.sh) | The script the log rotation job runs |
| [`docs/RUNBOOKS/network-exposure.md`](../docs/RUNBOOKS/network-exposure.md) | Which ports are public, and how to change that |

## Before you start

- **A network receiver.** The recipes are written for a KiwiSDR, which
  every build supports and which needs no device permissions. An HPSDR
  radio on the network works the same way. None of the recipes gives manta
  access to a local sound card or USB SDR. Release binaries include
  KiwiSDR and HPSDR support, not SoapySDR.
- **Linux:** the binary needs the ALSA runtime library even when it never
  opens a sound card: `sudo apt install libasound2` on Debian, Ubuntu and
  Raspberry Pi OS (`libasound2t64` on Debian 13).
- **macOS:** release binaries are not signed or notarized. If macOS
  refuses to run `./manta` because it was downloaded from the internet,
  clear the flag with `xattr -d com.apple.quarantine ./manta`.
- **Source checkout:** build with
  `cargo build --release -p manta-cli --features hpsdr` and use
  `target/release/manta` wherever a command below says `./manta`.

## Configure first

Every recipe needs an edited `manta.toml`. Make it before you install
anything:

```sh
cp manta.example.toml manta.toml
```

Copy the example, replace N0CALL with your station callsign, and configure your receiver. For audio or WAV input, set the actual radio dial frequency in input.center_freq_hz or pass --dial-freq-hz. Never use the example frequency as a substitute for your receiver's frequency.

**Station.** Under `[server]`, edit the active `station_callsign = "N0CALL"`
line, not the commented reference line above it. Your call is sent as the
spotter of every spot; an RBN SSID such as `-1` is allowed. The
`operator_*` lines are optional.

**Receiver.** For a KiwiSDR, find these lines in the `[input]` table that
is already in the file, delete the `#` at the start of each, and edit the
values. Do not add a second `[input]` table: a duplicate table is a TOML
error.

```toml
[input]
type = "kiwi"
host = "<your-receiver-host>"
port = 8073
freq_hz = 7030000.0
```

Replace `<your-receiver-host>` with your receiver's host name or address,
change `port` if it is not 8073, and set `freq_hz` to the frequency, in Hz,
you want the receiver tuned to. Set `password` only if the receiver needs
one. For an HPSDR radio, set `type = "hpsdr"`, `host`, `freq_hz` and
`rate_hz` instead; its `port` defaults to 1024.

**Check it.**

```sh
./manta config check --config manta.toml
```

It prints `manta.toml: valid (from --config)` and one line per table with
the settings it resolved, plus notes on stderr, such as that telnet and
JSON listen on every interface. When something is wrong it exits 1 naming
the setting, including any `<...>` example value you uncommented but did
not edit. It does not contact the receiver: a wrong host name or port
shows up only when manta runs.

**Uplinks.** Leave the `[[rbn_uplink]]` block commented out. If you enable
an uplink later, `dry_run` defaults to `true`: it connects and logs in but
sends no spots until you set `dry_run = false`.

**If `N0CALL` is left in place**, manta refuses to start:

```console
$ ./manta run --config manta.toml
Error: manta.toml: server.station_callsign is still the example "N0CALL" -- set your own callsign
```

It exits with status 1 before it opens the receiver or any listener;
`manta listen` behaves the same, and so does an uncommented
`[[rbn_uplink]]` block whose `login_callsign` is still `N0CALL`. The check reads the effective value, so a
`MANTA_SERVER_STATION_CALLSIGN` variable holding your call also satisfies
it. Under a service manager the service fails, is started again, and logs
this line each time until you fix the config.

**What the service logs.** manta writes a `SPOT:` line to stdout for each
confirmed spot, and its startup, status, warning and error lines to
stderr. It does not log decoded text; add `--decoded-text` to the command
line to include it, one line per track.

## Linux: systemd

Requires systemd 247 or newer (`systemctl --version`). Install the binary
and your edited config, check the installed config, then install and start
the unit:

```sh
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo install -d -m 0755 /etc/manta
sudo install -m 0600 manta.toml /etc/manta/manta.toml
sudo /usr/local/bin/manta config check --config /etc/manta/manta.toml
sudo install -m 0644 packaging/systemd/manta.service /etc/systemd/system/manta.service
sudo systemd-analyze verify /etc/systemd/system/manta.service
sudo systemctl daemon-reload
sudo systemctl enable --now manta
journalctl -u manta -f
# Stop, including while Restart=always is configured:
sudo systemctl stop manta
```

The `install` commands print nothing on success, and `systemd-analyze
verify` must report no errors for the unit.

- **Config access.** `DynamicUser=yes` runs manta as a temporary user that
  exists only while the service runs. `LoadCredential=manta.toml:/etc/manta/manta.toml`
  has systemd, which runs as root, copy the root-owned mode-0600 config
  into a private credentials directory that only the service can read, and
  the unit runs `manta run --config ${CREDENTIALS_DIRECTORY}/manta.toml`.
  A receiver password in the config is never world-readable. The copy is
  made each time the service starts: after editing
  `/etc/manta/manta.toml`, run the `config check` line again, then
  `sudo systemctl restart manta`.
- **Absolute paths.** manta resolves a relative path in the config against
  the config's directory, and at run time that is the credentials
  directory, not `/etc/manta`. Give `path` under `[input]`, and
  `blocklist_path` and `notch_path` under `[spot]`, as absolute paths to
  files any user can read, outside `/tmp`: the service gets a private
  `/tmp` of its own.
- **No local devices.** The temporary user belongs to no device group such
  as `audio` or `plugdev`, so this unit cannot open a sound card or USB
  SDR. Use a network receiver. The secondary-skimmer field-node runbook
  (MAN-96) has a separate unit with a static `manta` user for a local
  SDR; it is not part of this kit.
- **Restarts.** `Restart=always` starts manta again 10 seconds
  (`RestartSec=10`) after it exits for any reason, including a config
  error or a receiver it cannot reach at startup, and
  `StartLimitIntervalSec=0` means systemd never gives up. `systemctl stop`
  does not trigger a restart.
- **Stopping.** `systemctl stop` and `systemctl restart` send SIGTERM.
  manta stops decoding, gives connected clients up to 50 seconds to drain,
  and gives its runtime up to 2 more seconds to shut down.
  `TimeoutStopSec=60` leaves a margin before systemd sends SIGKILL.
- **Logs.** stdout and stderr go to the journal under the identifier
  `manta`. Reading them with `journalctl -u manta` needs root or
  membership in the `systemd-journal` or `adm` group. journald keeps them
  within the host's limits: `SystemMaxUse=` for the persistent journal in
  `/var/log/journal` and `RuntimeMaxUse=` for the volatile one in
  `/run/log/journal`, set in `/etc/systemd/journald.conf` or a drop-in
  under `/etc/systemd/journald.conf.d/`. Each defaults to 10% of its file
  system, capped at 4 GiB. The kit installs no journald override.
  `journalctl --disk-usage` shows what the journal uses now. The journal
  gets plain text, with no colour codes. To change the level or format,
  run `sudo systemctl edit manta` and add a drop-in that clears
  `ExecStart=` and sets it again with `-q` or `--log-format json`
  appended, or that sets `Environment=RUST_LOG=warn`; see the `## Logs`
  section of the top-level README.md.

## macOS: LaunchDaemon

The LaunchDaemon starts manta at boot as the service account `_manta`,
with nobody logged in. It is the only macOS setup this kit supports.
Create the account first: both plists name `_manta`, and launchd cannot
start a job whose account does not exist.

```sh
sudo sh packaging/launchd/create-service-account.sh
sudo install -d -m 0755 /usr/local/bin /usr/local/libexec
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo install -d -o root -g _manta -m 0750 /usr/local/etc/manta
sudo install -o root -g _manta -m 0640 manta.toml /usr/local/etc/manta/manta.toml
sudo install -d -o _manta -g _manta -m 0750 /var/log/manta
sudo -u _manta /usr/local/bin/manta config check --config /usr/local/etc/manta/manta.toml
sudo install -o root -g wheel -m 0755 packaging/launchd/rotate-log.sh /usr/local/libexec/manta-rotate-log.sh
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta.plist /Library/LaunchDaemons/
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta-logrotate.plist /Library/LaunchDaemons/
plutil -lint /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
plutil -lint /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
sudo launchctl print system/com.hagaletechnologies.manta
# Stop the daemon before unloading its log rotation job:
sudo launchctl bootout system/com.hagaletechnologies.manta
sudo launchctl bootout system/com.hagaletechnologies.manta-logrotate
```

The account script prints `manta service account ready: _manta`, the
`install` commands print nothing, and `plutil` prints `<path>: OK` for
each file.

- **Service account.** `create-service-account.sh` creates a `_manta` user
  and group with a free ID between 400 and 499, no login shell
  (`/usr/bin/false`), home `/var/empty` and no password. Running it again
  accepts an existing matching account. If `_manta` exists in another
  form, is only partly set up, no ID in the range is free, or a directory
  command fails, it exits non-zero with a message on stderr and deletes or
  overwrites nothing.
- **Config.** `/usr/local/etc/manta/manta.toml` is owned by root, group
  `_manta`, mode 0640: the service can read it and other users cannot. The
  `sudo -u _manta` check proves the service account can read it. Relative
  paths in the config resolve against `/usr/local/etc/manta`, and any file
  it names must be readable by `_manta`. To change the config, edit it with
  `sudo -e /usr/local/etc/manta/manta.toml`, run the check line again, and
  restart manta.
- **Stopping and restarting.** `KeepAlive` makes launchd start manta again
  whenever it exits, at most once every 10 seconds (`ThrottleInterval`).
  Stop it with `launchctl bootout`, not `kill`: after a `kill`, launchd
  just starts a new manta. `bootout` sends SIGTERM and waits up to
  `ExitTimeOut`, 60 seconds, before SIGKILL; manta needs at most 50
  seconds to drain its clients plus 2 seconds of runtime shutdown. To
  restart, boot the daemon out and bootstrap it again. Boot out the daemon
  before its rotation job.
- **Logs.** launchd writes manta's stdout and stderr to
  `/var/log/manta/manta.log`; follow it with
  `sudo tail -f /var/log/manta/manta.log`. The rotation job runs as
  `_manta` once when it is loaded and then once a minute. When the log is
  larger than 10 MiB, it keeps the final 1 MiB in place: it copies that
  tail to a temporary file in the same directory, overwrites the log with
  it, and removes the temporary file. The log keeps its inode because
  manta never reopens its stdout and stderr, and SIGHUP shuts manta down
  instead of reopening anything. A rename-based rotator such as newsyslog
  would leave manta writing to the renamed file. Limits: the log can grow
  past 10 MiB between runs, or while the job cannot run, and lines manta
  writes during the copy can be lost. This is operational log retention,
  not a strict disk quota or a lossless audit log. The log gets plain
  text, with no colour codes. To change the level or format, add `-q` or
  `--log-format json` to the installed plist's `ProgramArguments`.
- **Checking rotation.** The job discards its own output, so a failure
  shows only in its exit status:
  `sudo launchctl print system/com.hagaletechnologies.manta-logrotate`
  reports the last exit code, which is 0 after a run that rotated the log
  or had nothing to do.

## Docker Compose

`docker-compose.yml` runs manta in a container with your `manta.toml`
mounted read-only.

**Image.** The file names `ghcr.io/hagaletechnologies/manta:latest`. No
release has been published, so that image does not exist yet. Until it
does, build the image in a source checkout, which has the `Dockerfile`
that release archives lack:

```sh
docker build -t manta:local .
```

Then change the `image:` line in `docker-compose.yml` to
`image: manta:local`.

**Config.** Create and edit `manta.toml` next to `docker-compose.yml`
first, as in [Configure first](#configure-first). The container runs
manta as the non-root user `manta`, which the image's Dockerfile creates
with `useradd --system`; its UID and GID are not fixed. The mount keeps
the host file's owner and mode, so that user must be able to read the
file. For a public receiver with no password, make it world-readable:

```sh
chmod 0644 manta.toml
```

Before you store a receiver password in it, make it readable by the
container user's group only. This asks the image for that group's ID:

```sh
gid=$(docker compose run --rm -T --entrypoint id manta -g)
sudo chgrp "$gid" manta.toml
chmod 0640 manta.toml
```

**Run it.**

```sh
docker compose config
docker compose up -d
docker compose logs -f
# Stop and remove the container:
docker compose down
```

`docker compose config` prints the resolved file, or an error if the file
is invalid.

- **The mount.** `./manta.toml` is bound read-only at
  `/etc/manta/manta.toml`, and the service runs
  `manta run --config /etc/manta/manta.toml`. With
  `create_host_path: false`, a missing `./manta.toml` stops the container
  from starting instead of Docker creating an empty directory in its
  place. Only that file is mounted: a blocklist, notch list or WAV file
  the config names needs a mount of its own, at the path the config gives.
- **SELinux.** `selinux: Z` matters only on a host that enforces SELinux,
  such as Fedora or RHEL. There Docker relabels `./manta.toml` on the host
  with a private label for this container, so this container can read it
  and other containers cannot. The label stays on the host file. Keep the
  file dedicated to this container, and never point a `Z` mount at a
  shared file or directory.
- **Ports.** Telnet (7300) and JSON (7301) are published on every host
  interface, as a public cluster node expects. Inside the container manta
  binds metrics to `0.0.0.0` (`MANTA_SERVER_METRICS_BIND_ADDR`, which
  overrides `metrics_bind_addr` in `manta.toml`), because a published port
  reaches the container's network interface, not its loopback. The host
  publishes metrics only on `127.0.0.1:7302`. If you change `telnet_port`,
  `json_port` or `metrics_port` in `manta.toml`, change the container side
  (the right-hand number) of the matching `ports:` entry too. Keep
  `bind_addr` at its default inside the container: `127.0.0.1` there makes
  telnet and JSON unreachable through the published ports.
- **Stopping and restarts.** `docker compose down` and
  `docker compose stop` send SIGTERM, Docker's default stop signal, and
  wait up to `stop_grace_period: 60s` before SIGKILL, longer than manta's
  50-second client drain plus 2 seconds of runtime shutdown.
  `restart: unless-stopped` starts manta again after it exits and when
  Docker starts, unless you stopped it.
- **Logs.** The `json-file` driver rotates the container's log at 10 MB
  and keeps 3 files (`max-size: "10m"`, `max-file: "3"`), about 30 MB in
  all. `RUST_LOG=info` sets the log level. For JSON log lines, append
  `"--log-format", "json"` to `command:`.
- **Not enabled.** No device passthrough, privileged mode or host
  networking: use a network receiver.

## Windows

The Windows archive has `manta.exe` and the same reference files. This kit
has no Windows service wrapper. To check the config and run manta in the
foreground, use `.\manta.exe config check --config manta.toml` and then
`.\manta.exe run --config manta.toml` in a PowerShell window; Ctrl+C stops
it.

## Network exposure

By default telnet (7300) and JSON (7301) accept connections on every
interface (`bind_addr = "0.0.0.0"`), as a public cluster node does. The
metrics endpoint (7302, `/metrics` and `/healthz`) has no password and
accepts connections only from the same machine
(`metrics_bind_addr = "127.0.0.1"`). With any of the three setups,
`curl -s http://127.0.0.1:7302/healthz` on that machine reports the node's
health. Read [network-exposure.md](../docs/RUNBOOKS/network-exposure.md)
before you change either address.

## Upgrading or reinstalling

Stop the service, replace the binary and any kit files that changed, keep
your edited config, check it, and start again. Do not repeat the step that
installs `manta.toml`: it would replace your config. Compare the new
`manta.example.toml` with your config for settings a release adds;
settings you left commented out follow the new release's defaults.

systemd:

```sh
sudo systemctl stop manta
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo install -m 0644 packaging/systemd/manta.service /etc/systemd/system/manta.service
sudo systemctl daemon-reload
sudo /usr/local/bin/manta config check --config /etc/manta/manta.toml
sudo systemctl start manta
```

macOS (the account script only confirms the existing account here):

```sh
sudo launchctl bootout system/com.hagaletechnologies.manta
sudo launchctl bootout system/com.hagaletechnologies.manta-logrotate
sudo sh packaging/launchd/create-service-account.sh
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo -u _manta /usr/local/bin/manta config check --config /usr/local/etc/manta/manta.toml
sudo install -o root -g wheel -m 0755 packaging/launchd/rotate-log.sh /usr/local/libexec/manta-rotate-log.sh
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta.plist /Library/LaunchDaemons/
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta-logrotate.plist /Library/LaunchDaemons/
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
```

Docker Compose: run `docker compose down`, rebuild `manta:local` from the
updated checkout (or, once a released image exists, run
`docker compose pull`), then `docker compose up -d`.

## Removing a service

Removing a service does not delete its config, its binary or, on macOS,
its account and log. Delete them yourself if you no longer need them.

systemd:

```sh
sudo systemctl disable --now manta
sudo rm /etc/systemd/system/manta.service
sudo systemctl daemon-reload
```

This leaves `/etc/manta/manta.toml` and `/usr/local/bin/manta`. The unit's
temporary user goes away with the service.

macOS:

```sh
sudo launchctl bootout system/com.hagaletechnologies.manta
sudo launchctl bootout system/com.hagaletechnologies.manta-logrotate
sudo rm /Library/LaunchDaemons/com.hagaletechnologies.manta.plist /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
```

This leaves `/usr/local/bin/manta`, `/usr/local/libexec/manta-rotate-log.sh`,
`/usr/local/etc/manta`, `/var/log/manta` and the `_manta` account;
`sudo dscl . -delete /Users/_manta` and `sudo dscl . -delete /Groups/_manta`
remove the account.

Docker Compose: `docker compose down` removes the container and leaves
`manta.toml` and the image.
