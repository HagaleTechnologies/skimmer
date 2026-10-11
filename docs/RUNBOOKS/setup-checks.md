# Runbook: check a manta setup with `manta doctor`

`manta doctor` checks that a machine is ready to run manta, then whether the
receiver hears anything. Run it before you install manta as a service, and
again whenever something changes: a new config, a new receiver, a new
network.

```sh
manta doctor --config /etc/manta/manta.toml
```

Run it with the service **stopped**. A running `manta run` holds its own
ports and its receiver, so doctor would report the ports as in use and could
not open a sound card or SDR that only one program may hold.

Decision record:
[2026-10-10-man126-doctor-setup-checks](../DECISIONS/2026-10-10-man126-doctor-setup-checks.md).

## What it prints

One line per check, in this order, each starting with `PASS`, `WARN`, `FAIL`
or `SKIP`. Every `WARN` and `FAIL` line is followed by an indented `fix:`
line saying what to do. Then comes the signal report (track, SNR and spot
counts and a verdict), and a closing summary that names every failed and
warned check:

```console
$ manta doctor --config manta.toml
PASS  config: manta run's start-up checks accept manta.toml
PASS  audio: ALSA runtime library is installed; this KiwiSDR receiver does not use a sound card
FAIL  telnet port: 0.0.0.0:7300 is already in use
      fix: stop the program listening on port 7300 (on Linux, `sudo ss -ltnp 'sport = :7300'` names it) or set a free telnet_port in [server]; if it is a running manta, check it with `manta status` instead
PASS  json port: 0.0.0.0:7301 is free
PASS  metrics port: 127.0.0.1:7302 is free
PASS  clock: within 0.004 s of pool.ntp.org, and NTP is keeping it in sync
PASS  rbn uplink telnet.reversebeacon.net:7000: accepted a connection in 36 ms
PASS  receiver: opened KiwiSDR kiwi.example.net:8073 (12000 Hz)

source: 12000 Hz sample rate, 7030000.0 Hz center, observed for 10.0s
...
verdict: DECODING -- at least one confirmed spot. End to end, working.

doctor: 1 failed: telnet port (7 passed)
```

All check lines go to stdout. With `--json`, doctor prints one JSON object
instead: the signal report's keys, plus `checks` (one object per line, with
`name`, `status`, `detail` and `fix`) and `checks_status` (`fail` if any
check failed, else `warn` if any warned, else `pass`). When the receiver does
not open, the object holds only `checks` and `checks_status`.

## The checks

| Check | What it proves | What it does not prove |
|---|---|---|
| `config` | `manta run` will not refuse this config for either reason it checks before starting: the example `N0CALL` callsign in `server.station_callsign` or `rbn_uplink.login_callsign`, or a `[server]` table with a receiver that reports no radio frequency and no dial frequency set. `SKIP` with neither a config file nor `MANTA_SERVER_*` variables | Everything `manta config check` checks; a config that does not load at all stops doctor with an `Error:` line, as before |
| `audio` | The audio library loaded. For a sound-card receiver, the audio system lists at least one input; the line names up to four | That the listed input is the radio. On a headless Linux host the list can hold only ALSA's "Discard all samples" null device |
| `telnet port`, `json port`, `metrics port` | Each `[server]` listener can bind its address and port right now. Two listeners on one port fail without binding. Port 0 is a `SKIP`: the system picks a free port when `manta run` starts | That the port is still free when `manta run` starts: another program can take it in between |
| `ports` | One `SKIP` row instead of the three above when there is no `[server]` table, because `manta run` then starts no servers | |
| `clock` | This clock is within 1 s of the NTP server (`--ntp-server`, default `pool.ntp.org`). On Linux, the kernel also reports that a time daemon (chrony, systemd-timesyncd or ntpd) is keeping it in sync | That the server itself is right |
| `rbn uplink <host:port>` | Each enabled `[[rbn_uplink]]` target resolves and accepts a TCP connection within 10 s. Disabled targets are a `SKIP`. With no `[[rbn_uplink]]` block, one `SKIP` row named `rbn uplink` | That the collector accepts your callsign. Doctor sends nothing on the connection: no login, no callsign, no spot. Once the daemon runs, `manta status` shows the login (see [uplink-health](uplink-health.md)) |
| `receiver` | The configured receiver opens, at the sample rate shown | That it hears anything; the signal report answers that |
| `signal` | Printed only when the receiver opened but the decode run stopped with an error | |

### Clock thresholds

| Offset from the NTP server | Status |
|---|---|
| under 1 s | `PASS` (on Linux, `WARN` if nothing is keeping the clock in sync, because it will drift) |
| 1 s to under 60 s | `WARN`: spot times are off by that much |
| 60 s or more | `FAIL`: every spot carries the wrong minute |
| server did not answer within 3 s | `WARN`, or `PASS` on Linux when the kernel reports NTP sync |

The fix for a wrong or unsynchronized clock depends on the platform:

- Linux: `sudo timedatectl set-ntp true`, or install and start chrony.
- macOS: System Settings > General > Date & Time > Set time and date
  automatically.
- Windows: Settings > Time & language > Date & time > Set the time
  automatically.

When outbound NTP is blocked, name a server on your own network:
`manta doctor --ntp-server ntp.example.lan`, or `HOST:PORT` for a
non-standard port.

### Fixes by failure

| Line | Fix |
|---|---|
| `config: ... is still the example "N0CALL"` | Set that key to your own callsign in the config file |
| `config: this receiver reports no radio frequency ...` | Set `input.center_freq_hz`, or pass `--dial-freq-hz`, to the radio's dial frequency in Hz |
| `audio: the audio system lists no sound-card inputs` | Connect the radio's audio interface. On Linux, `arecord -l` must list it, and the user running manta must be in the `audio` group |
| `audio: the audio system could not list inputs` | On Linux, reinstall the ALSA runtime (`sudo apt install --reinstall libasound2`, or `libasound2t64` on Debian 13) and check that `arecord -l` works for this user |
| `<name> port: ... is already in use` | Stop the program on that port (`sudo ss -ltnp 'sport = :<port>'` names it on Linux) or choose a free port. If it is a running manta, use `manta status` instead |
| `<name> port: ... is not an address of this machine` | Set `bind_addr` (or `metrics_bind_addr` for the metrics port) to one of this machine's addresses, or `0.0.0.0` |
| `<name> port: this user may not listen on port ...` | Use a port above 1023, or grant `CAP_NET_BIND_SERVICE` (`AmbientCapabilities=` in the systemd unit) |
| `<name> port: ... is the same as ...` | Give each server its own port |
| `rbn uplink ...: could not look up <host>` | Check `target_host`, and this machine's DNS (`nslookup <host>`) |
| `rbn uplink ...: refused the connection` | Check `target_port`; otherwise the collector is down or not accepting connections |
| `rbn uplink ...: did not answer within 10 s` | A firewall may be dropping outbound TCP to that port |
| `receiver: could not open ...` | Sound card: connect it, or choose one with `--device`/`input.device`. WAV file: check `--source`/`input.path`. KiwiSDR: check `--kiwi-host`/`--kiwi-port` and that it has a free channel. SoapySDR: check `--soapy-driver` and `SoapySDRUtil --find`. HPSDR: check `--hpsdr-host` and that the radio is on this network |
| `signal: the receiver opened, but the signal check stopped` | A WAV file must hold at least 3 seconds of audio; a live receiver must keep delivering samples |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | No check failed. Warnings are allowed, and the signal verdict (including `NO_SIGNAL`) does not change the exit code: a quiet band is not a broken setup |
| 1 | At least one check failed, or doctor could not start (a bad `--duration`, a config that does not load) |
| 2 | Command-line usage error |

## Network traffic doctor sends

Allow these outbound if a firewall sits between the node and the internet:

- One SNTP request (48 bytes, UDP port 123) to the `--ntp-server` host.
- One TCP connection to each enabled `[[rbn_uplink]]` target, closed as soon
  as it opens.

## manta will not start at all

On Linux, the manta binary needs the ALSA runtime library even when it never
opens a sound card. When the library is missing, the operating system's
loader refuses to start manta before any of its own code runs, so doctor
cannot report it. The loader names the library itself:

```console
$ manta doctor
manta: error while loading shared libraries: libasound.so.2: cannot open shared object file: No such file or directory
```

Install it, then run doctor again:

```sh
sudo apt install libasound2        # Debian 12, Ubuntu, Raspberry Pi OS
sudo apt install libasound2t64     # Debian 13
```
