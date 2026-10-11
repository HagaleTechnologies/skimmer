<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img src="assets/logo-light.svg" alt="manta" width="160">
  </picture>
</p>

<h1 align="center">manta</h1>

<p align="center">
  Open-source wideband CW skimmer. Every CW signal in an SDR passband, decoded at once,
  emitted as RBN-compatible spots.
</p>

<p align="center">
  <a href="https://github.com/HagaleTechnologies/manta/actions/workflows/ci-full.yml"><img alt="CI" src="https://github.com/HagaleTechnologies/manta/actions/workflows/ci-full.yml/badge.svg"></a>
  <a href="https://github.com/HagaleTechnologies/manta/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/HagaleTechnologies/manta"></a>
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg">
  <img alt="Rust 1.98+" src="https://img.shields.io/badge/rust-1.98%2B-orange.svg">
</p>

`manta` is a headless daemon written in Rust. It takes wideband IQ from a
commodity SDR, an OpenHPSDR/Hermes device, a KiwiSDR over the network, or a
WAV file, channelizes the whole passband with a polyphase filterbank, runs an
independent CW decoder on every signal it finds, validates the callsigns, and
emits spots. Output is the standard `DX de` cluster format over telnet plus a
JSON Lines / WebSocket stream, so existing aggregators (including the Reverse
Beacon Network) and modern consumers (such as [cqdx](https://cqdx.app)) can
ingest it without changes.

No GUI. CLI, a TOML config file, and Prometheus metrics.

```console
$ telnet manta.example.org 7300
login: W1XYZ
de W5AU-# >
DX de W5AU-#:   14000.70  W1AW           CW     7 dB  20 WPM  CQ      1533Z
```

## Why

The Reverse Beacon Network is infrastructure the whole amateur radio hobby
leans on for CW spotting, contest scoring, propagation awareness, and antenna
testing. Nearly all of it is skimmed by
**[CW Skimmer](https://www.dxatlas.com/CwSkimmer/)** and
**[CW Skimmer Server](https://www.dxatlas.com/SkimServer/)**, written by Alex
Shovkoplyas, VE3NEA — the reference implementation of wideband CW skimming,
and the reason the network works as well as it does.

Those are Windows programs, and Skimmer Server's own system requirements ask
for an x86 CPU with SSE3, so there is no native build for the platforms a lot
of amateur radio now runs on: Linux, ARM, a headless Raspberry Pi at a remote
antenna. `manta` is a second, independent implementation with native builds
for exactly those platforms — and for macOS and Windows as well — a headless
daemon, open source, with documented algorithms and golden-vector regression
tests anyone can read, run, and check. Shared infrastructure is healthier with
more than one implementation of it, on more than one operating system.

## Installation

There is no tagged release yet, so there is no prebuilt binary or Docker
image to pull — build from source. You need Rust 1.98+ and a `git`
executable on `PATH`. Git is a build-time requirement, not just a way to
clone this repo: manta depends on
[`coppa`](https://github.com/HagaleTechnologies/coppa) as a rev-pinned git
dependency, and `.cargo/config.toml` sets `[net] git-fetch-with-cli = true`
so cargo fetches it through the `git` binary rather than its built-in
libgit2 transport. Without git on `PATH` the build fails at the fetch step,
before compiling anything.

```sh
git clone https://github.com/HagaleTechnologies/manta
cd manta
cargo install --path crates/manta-cli --features hpsdr
```

That puts a `manta` binary in Cargo's bin directory (`~/.cargo/bin`
unless you moved `CARGO_HOME`), which a standard Rust install already has
on `PATH` — every command below assumes a bare `manta` resolves. To build
without installing, `cargo build --release -p manta-cli --features hpsdr`
leaves the binary at `target/release/manta`; run that path instead. Carry
the `--features hpsdr` across: drop it and the binary has no `--hpsdr-host`
flag at all, so it no longer matches the install above or the HPSDR row in
the Inputs table below.

A binary and a Docker image publish automatically, for every platform,
from the first tag:

```sh
# once a release exists:
docker run --rm ghcr.io/hagaletechnologies/manta:latest --help
```

**Notes:**
- Building on Linux compiles the ALSA bindings (audio input is an
  unconditional dependency, even if you only ever use file, KiwiSDR, or
  HPSDR input), so the build host needs the ALSA development headers and
  `pkg-config` — `sudo apt install libasound2-dev pkg-config` on
  Debian/Ubuntu/Raspberry Pi OS, or the equivalent `alsa-lib` devel
  package elsewhere. A machine that only *runs* a binary built elsewhere
  needs just the ALSA runtime library (`libasound2`, `libasound2t64` on
  Debian 13).
- Stop a long-running container with `docker stop -t 60 <container>`.
  Docker's own default 10-second grace period before SIGKILL is far
  shorter than manta's graceful-shutdown window: the daemon's internal
  cutoff (`SHUTDOWN_DRAIN_DEADLINE`) is 50 s, so 60 s on the caller side
  lets the daemon always reach its own cutoff first and record what it
  abandoned on `manta_spots_dropped_write_failed_total` instead of being
  truncated silently. Use the same 60 s for `--stop-timeout` on `docker
  run`, `stop_grace_period` on Compose, and `terminationGracePeriodSeconds`
  on Kubernetes.
- Input backends are cargo features, and a build that did not ask for
  one has no flags for it — `--hpsdr-host` / `--soapy-driver` fail with
  `error: unexpected argument` on a build without them. `hpsdr`
  (OpenHPSDR/Hermes) has no native dependency, which is why the install
  line above turns it on. `soapy` (RTL-SDR, Airspy, SDRplay, HackRF via
  SoapySDR) needs the native SoapySDR system library installed first;
  once you have it, add it: `--features hpsdr,soapy`.
- Windows binaries from the release workflow link the MSVC runtime
  statically, so they need no Visual C++ Redistributable. A Windows build
  from source links it dynamically unless you set
  `RUSTFLAGS="-C target-feature=+crt-static"` yourself, so a binary built
  that way needs the [Visual C++
  Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)
  on any machine that doesn't already have it.
- In a container, the metrics endpoint (`/metrics`, `/healthz`) listens on
  the container's own loopback by default, which a published port cannot
  reach. To scrape or probe it through `-p`, set
  `MANTA_SERVER_METRICS_BIND_ADDR=0.0.0.0` and publish it as
  `-p 127.0.0.1:7302:7302`, which keeps it on the host — see
  [docs/RUNBOOKS/network-exposure.md](docs/RUNBOOKS/network-exposure.md).
- If the `docker run` above returns an authorization error, the GHCR
  package still needs its one-time "make public" step — see
  [docs/RUNBOOKS/release.md](docs/RUNBOOKS/release.md).
- Every release also carries a `SHA256SUMS` file and a GitHub
  build-provenance attestation. Check a download before running it:
  [Verifying a downloaded release](docs/RUNBOOKS/release.md#verifying-a-downloaded-release).

## 60-second demo

No SDR, no radio. With `manta` on `PATH` from the install above, generate
a synthetic golden vector and decode it:

```sh
manta gen v1 --out /tmp/v1              # 120 s of synthetic CW, 20 WPM, +20 dB
manta decode /tmp/v1/v1.wav
# CQ DE W1AW W1AW K CQ CQ DE W1AW W1AW K CQ CQ DE W1AW W1AW K CQ …
```

Then point it at a real signal — a public KiwiSDR needs no hardware of
your own. `<your-kiwi-host>` is a placeholder: pick a receiver that covers
the band you want from the public KiwiSDR directory at
<https://kiwisdr.com/public/> (or the map at <http://rx.linkfanel.net/>)
and substitute its hostname and port — there is no default receiver and
the command will not connect until you do. `--kiwi-port` defaults to 8073,
the standard KiwiSDR port; a receiver the directory lists on any other port
needs it spelled out, so it is a placeholder here too and you can drop the
flag entirely when yours is on 8073:

```sh
manta listen --kiwi-host <your-kiwi-host> --kiwi-port <your-kiwi-port> --kiwi-freq-hz 7030000
```

File replay (`listen --source`) takes 48 kHz mono audio, or a raw complex-IQ
WAV with `--source-iq`. IQ replay is not resampled: the channelizer runs
only at its table rates — any rate whose `fs / 93.75` is a power of two,
which among the rates recorders commonly produce means 24, 48, 96, 192,
384 and 768 kS/s — and `--source-iq` only changes how the WAV is
interpreted, so a recording at an off-table rate (100 or 250 kS/s, say) is
rejected with `unsupported sample rate …: fs/93.75 must be a power of two`.
That list is examples, not the whole set: divide your own rate by 93.75,
and it replays if the answer is a power of two of at least 4.
`--capture-rate-hz` decimates by a power of two into a *lower* table rate
(`--capture-rate-hz 48000` on a 192 kS/s capture, to spend less CPU on a
narrower passband); it is not a general resampler and cannot rescue an
off-table recording. Replay runs faster than realtime, so the hardware-free
path above stops at `decode`; a paced replay that can drive the servers
below is being worked on.

`manta --help` lists every subcommand and flag.

## Run it as a node

One TOML file describes a node: the station and servers in `[server]`, the
receiver in `[input]`. With a `[server]` table, `run --config` starts the
DX cluster telnet server, the JSON/WebSocket stream, and the metrics
endpoint alongside the decoder:

```toml
# manta.toml
[server]
station_callsign = "W5AU"   # your call; becomes `DX de W5AU-#:` and JSON `deCall`
bind_addr = "127.0.0.1"     # telnet + JSON; 0.0.0.0 to accept remote clients
metrics_bind_addr = "127.0.0.1"  # the default; metrics stays on this machine
telnet_port = 7300
json_port   = 7301
metrics_port = 7302

[input]
type = "kiwi"               # audio | file | kiwi | soapy | hpsdr
host = "<your-kiwi-host>"   # a receiver from the public directory above
port = 8073   # <your-kiwi-port>: replace if your receiver is not on 8073
freq_hz = 7030000.0
```

To start from a file that lists every setting instead, `manta config init`
writes a `manta.toml` with each one commented out at its default and
explained. Before you run a file, `manta config check` validates it,
including any `MANTA_*` variables, and prints the settings it resolves to.
It opens no receiver and no port, and exits non-zero naming the setting
and the problem when something is wrong:

```sh
manta config init                         # writes ./manta.toml; never replaces one
manta config check --config manta.toml
```

Then `manta doctor` checks the machine itself: the audio library, that the
server ports are free, the clock against an NTP server, that each enabled
RBN uplink target accepts a connection, and that the receiver opens and
hears something. Each check prints `PASS`, `WARN`, `FAIL` or `SKIP` with a
fix for every problem, and it exits 1 when a check fails. See
[docs/RUNBOOKS/setup-checks.md](docs/RUNBOOKS/setup-checks.md).

```sh
manta doctor --config manta.toml
```

Start the server in one terminal. It runs in the foreground until you stop it:

```sh
manta run --config manta.toml
```

Every flag still overrides the file, for one-off runs: `run --config
manta.toml --freq-correction-ppm 2.5` corrects this session only, and
`--source`, `--device` or `--kiwi-host` replace the whole `[input]` table.
Service managers can set `MANTA_CONFIG=/etc/manta/manta.toml` instead of
passing `--config`, and override single keys with `MANTA_<TABLE>_<KEY>`
variables (`MANTA_INPUT_FREQ_HZ=14030000`). `[spot]`, `[detector]` and
`[decode]` tables tune the rest;
[docs/SPEC-decode-core.md](docs/SPEC-decode-core.md) §9 lists every key,
its default and its precedence. An unknown table or key is an error, not
silently ignored.

Keep `cty.dat` current by downloading it and pointing `[spot] cty_path`
(or `--cty`) at it. manta warns at startup when its built-in copy is more
than 180 days old.

Then probe it from a second terminal:

```sh
telnet localhost 7300          # DX de … lines
nc localhost 7301              # one JSON object per spot
curl -s localhost:7302/metrics # Prometheus text
```

Check a running daemon's health at a glance (spot counts, RBN uplink
connection and reconnect state per target) instead of reading its logs:

```sh
manta status --config server.toml
```

Forwarding to an upstream RBN-style collector is a `[[rbn_uplink]]`
block. `dry_run` defaults to `true`, so an uplink connects and logs in but
transmits nothing until you set it to `false` deliberately.

`bind_addr` has no loopback default — omit it and the telnet and JSON
servers bind `0.0.0.0`, every interface, as a public cluster node expects.
The example above pins `127.0.0.1` on purpose. Metrics is the exception:
it listens on `metrics_bind_addr`, `127.0.0.1` by default, because it has
no password. Read
[docs/RUNBOOKS/network-exposure.md](docs/RUNBOOKS/network-exposure.md)
before you widen either.

## Running unattended

To run a node as a service that starts at boot and restarts after a
failure, use the kit in [packaging/README.md](packaging/README.md): a
systemd unit, a macOS LaunchDaemon and a Docker Compose file, with install
steps for each. Release archives carry the same files next to the binary.
Start from [manta.example.toml](manta.example.toml): copy it to
`manta.toml`, replace `N0CALL` with your station callsign, configure your
receiver, and run `manta config check --config manta.toml` before you
enable a service. `manta run` refuses to start while the station callsign
is still `N0CALL`.

## Inputs

| Source | How | Status |
| --- | --- | --- |
| IQ / audio WAV file | `decode`, `listen --source` (`--dial-freq-hz` sets absolute frequencies for audio files) | Working (`decode` takes IQ; `listen --source` takes 48 kHz mono audio, or raw complex IQ with `--source-iq`). IQ is never resampled: the rate must be one the channelizer supports (`fs / 93.75` a power of two — commonly 24, 48, 96, 192, 384, 768 kS/s, but the rule decides, not that list), and `--capture-rate-hz` only decimates from one of those to a lower one |
| Sound card (rig audio passband) | `listen --device`, `--dial-freq-hz` for absolute frequencies | Working, 48 kHz input only |
| KiwiSDR over the network | `listen --kiwi-host` — any receiver from the public directory at <https://kiwisdr.com/public/> | Working |
| OpenHPSDR / Hermes (Hermes-Lite 2, Red Pitaya, QMTech) | `listen --hpsdr-host`, feature `hpsdr` — on in the install line above, no native dependency | Working; protocol verified against reference sources, not yet against hardware |
| RTL-SDR, Airspy, SDRplay, HackRF, anything SoapySDR drives | `listen --soapy-driver`, feature `soapy` — **not** in the install line above; needs the SoapySDR system library, then `--features hpsdr,soapy` | Working, needs hardware soak |

The source frequency and rate flags end in `-hz`: `--kiwi-freq-hz`,
`--soapy-freq-hz`, `--soapy-rate-hz`, `--hpsdr-freq-hz` and
`--hpsdr-rate-hz`. The older `--kiwi-freq` / `--soapy-freq` /
`--soapy-rate` / `--hpsdr-freq` / `--hpsdr-rate` spellings still work.

### Check that samples reach manta

List inputs, check the selected source, then use `doctor` to assess decoding:

```sh
manta devices
manta check --device "USB Audio"
manta check --config manta.toml --json
manta check capture.wav --source-iq
manta doctor --config manta.toml --duration 10
```

Audio selectors are names matched case-insensitively by substring, not numeric
indexes. `devices` lists input-capable devices without opening a stream. With
`--features soapy`, it also lists complete Soapy selector strings for
`--soapy-driver`. HPSDR discovery is not supported; use `--hpsdr-host HOST`
with an `hpsdr` build. KiwiSDR uses `--kiwi-host HOST`. `devices --json` reports
empty, unavailable and failed enumeration separately; a backend error exits 1
while retaining the other results.

`check [SOURCE]` treats SOURCE as an audio WAV path, equivalent to `--source`.
Audio files must be mono at 48000 Hz; add `--source-iq` for complex stereo IQ.
Only one source selector is accepted. It replaces a configured input wholesale,
just like `run`. With no selector, `check` uses `--config`, then `MANTA_CONFIG`,
then environment/default settings; it does not search for `./manta.toml`.
`manta config check` validates configuration without opening a receiver;
`manta check` opens it and reads samples without decoding, listeners or uplinks.
It validates typed config but does not read unused spot asset files.

The default window is three seconds of delivered samples; `--duration` accepts
1 to 60 seconds. The report shows the actual **stream sample rate** after
resampling and `--capture-rate-hz` decimation, center frequency, passband,
received sample count, input power and a brief per-channel noise-floor estimate.
KiwiSDR's stream rate is 96000 Hz after resampling, not its native receiver rate.
Power is relative dBFS, not calibrated RF power, dBm or spot SNR. The floor is
the lower quartile in each eligible channel, summarized across the passband.
See the [measurement definition](docs/DECISIONS/2026-10-10-man125-source-diagnostics.md).

Digital silence is reported explicitly and can exit 0 because samples arrived.
It does not prove that an antenna is connected. Empty input or input too short
for a complete channelizer hop has no floor and exits 1; a shorter file with a
measured floor exits 0 and reports its actual duration and end of file. Errors
go to stderr; `--json` emits one report on stdout. A sampling deadline exits 1
and retains any measurements. The deadline is duration plus five seconds after
opening, checked between reads. Native open/read calls can exceed it, so
`--duration` is not a hard wall-clock timeout. No reconnect loop runs.

Targets Linux (x86-64 and ARM, Raspberry Pi 4 class), macOS, and Windows.
The CPU budget is a full 192 kS/s passband inside one Raspberry Pi 4 core,
enforced by criterion benches.

## Outputs

All four ship today. `manta run --config <file>` starts the telnet
server, the JSON/WebSocket stream and the metrics endpoint together when
the file has a `[server]` table; the outbound uplink starts alongside them
only if that config also carries at least one `[[rbn_uplink]]` block.

- **DX cluster telnet server** (`:7300`) — standard login prompt and
  RBN-format `DX de` lines, with enough command grammar (`sh/dx`,
  `set/dx/filter`) for stock clients. This is the RBN/aggregator
  compatibility surface.
- **JSON Lines / WebSocket stream** (`:7301`) — one full-fidelity spot
  object per line; a raw TCP client and a WebSocket client share the
  port. This is the [cqdx](https://cqdx.app) ingest surface.
- **Outbound RBN uplink** — forwards validated spots to an upstream
  collector in the same `DX de` wire format, with reconnect and multiple
  simultaneous targets. Working against a collector; **not yet validated
  against RBN's live ingest**.
- **Prometheus metrics** (`:7302/metrics`) — spot, client, uplink and
  source-health counters. Some gauges are still placeholders;
  ARCHITECTURE §8 says which.

`run` (alias `listen`) also prints a `SPOT:` line on stdout for each
confirmed spot, and decoded text on stderr, one line per track labelled
with its track number, frequency and speed. With a `[server]` table the
decoded text is off unless you pass `--decoded-text`, so a service log
holds spots and diagnostics only. `--json` prints every decoder event and
spot as JSON Lines on stdout instead. `decode` prints its decoded text on
stdout. This terminal output is a debugging aid, not a stable interface —
the servers above are.

The decode path is deterministic: the same file in produces byte-identical
spot logs out. That is a hard requirement, and CI enforces it with golden
test vectors.

### Outbound RBN uplink

manta can also log into an RBN spot-collection endpoint as a client and
forward its own spots there. Add one `[[rbn_uplink]]` block per target to
the daemon config:

```toml
[server]
station_callsign = "W3XYZ"

[[rbn_uplink]]
enabled = true
target_host = "rbn.example.org"
target_port = 7000
# dry_run defaults to TRUE: manta connects and logs in, so you can verify
# credentials and reachability, but transmits no spots. Set it to false
# only once you actually intend to feed a live target.
# dry_run = false
# spot_types defaults to "cq_beacon": only CQ and beacon spots, as RBN
# requires. "all" also sends DE and untyped spots.
# spot_types = "all"
```

Spots an uplink holds back still appear on manta's own telnet and JSON
output; `spot_types` only decides what goes to that target.

The uplink has not yet been verified against a real RBN ingest (see
[ROADMAP.md](ROADMAP.md)), which is why dry-run is the default. manta logs
which mode each target is in at startup.

## Status

Pre-1.0, and pre-first-release. What is true today:

- **Shipped:** the full wideband pipeline — polyphase channelizer,
  noise-floor detector, track manager, decoder pool — with five input
  sources, callsign validation (cty.dat, SCP, CQ/DE parsing, dedupe), and
  all four output surfaces above.
- **Not yet measured:** the RBN parity benchmark (recall vs. RBN on
  recorded contest IQ), the Raspberry Pi 4 CPU budget, and a 24-hour
  live-SDR soak. The first needs reference data; the other two need
  physical hardware. The CPU budget's desktop leg *has* been measured and
  currently reads as a **fail** — ≈0.53x–0.58x realtime against a <0.5x
  budget, pending a clean rerun on a quiet machine ([ROADMAP.md](ROADMAP.md) M2).
- **Known limits:** the classical decoder loses copy under heavy HF
  fading on several golden vectors, and at low SNR the validator still
  admits occasional bogus callsigns from noise. Closing the fading gap is
  classical-DSP work in flight, with ML fusion behind it at M4. The
  outbound RBN uplink is unverified against a real RBN ingest and ships
  dry-run by default until that verification lands.
- **Decode engine:** `decode.engine` defaults to `legacy`. A rewritten
  `hsmm` engine (decode-core-v2, `docs/SPEC-decode-core-v2.md`) is
  implemented and reachable via `--engine hsmm`, but its stage-2
  measurement gate (`docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md`)
  came back FAIL on 2026-09-09: real B2/K5TR oracle recall roughly doubles
  over `legacy` (`as_word` 27%→56%, `framed` 13%→32%) but falls short of
  the 60%/40% bar, and most VR/V golden vectors still fail. Not yet a
  default-engine candidate.
- **Measured sensitivity:** `manta bench sensitivity` regenerates recall
  and character error rate against SNR (500 Hz) on synthetic AWGN and
  Watterson-faded signals; the curve at landing is in
  [docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md](docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md).
- **No tagged release yet**, so the container image above is empty until
  the first tag.

[ROADMAP.md](ROADMAP.md) has the milestone breakdown with acceptance
criteria.

## Non-goals

- Not an interactive receiver or panadapter. Use SDR++ or similar for a
  waterfall.
- Not a general digital-mode skimmer. FT8 and RTTY are out of scope for 1.0,
  though the channelizer architecture does not preclude them later.
- Not a cluster network. `manta` is a spot source, not an aggregator.
- Not a logger. No QSO state.
- Not a multi-process orchestrator. `manta` is a single Rust binary, not a
  stack of programs to sequence-launch.
- No CW Skimmer-style dual MME/WDM soundcard configuration surface, and no
  CAT/rig control (OmniRig, live rig polling) to align a narrowband
  receiver with the channelizer. `manta` does ingest a local audio device
  (`listen`/`listen --device`, rig-audio passband), and that input mode
  accepts a manually-supplied dial frequency (`--dial-freq-hz`, MAN-34) so
  its spots report an absolute RF frequency rather than a baseband offset
  — entering the dial frequency once, not live rig polling. What remains a
  non-goal is CAT/OmniRig's live rig polling and the legacy Windows
  driver-selection and band-scope-alignment machinery around it, not the
  frequency reference itself. None of this applies to the wideband sources
  (OpenHPSDR/Hermes, SoapySDR, KiwiSDR), which already report their own
  tuned frequency and don't need CAT at all since the channelizer covers
  the whole passband at once.

## Documentation

**Running a node:** [docs/RUNBOOKS/network-exposure.md](docs/RUNBOOKS/network-exposure.md)
(exposing the servers safely) · the "Run it as a node" section above ·
[packaging/README.md](packaging/README.md) (running unattended under
systemd, launchd or Docker Compose) ·
[docs/RUNBOOKS/secondary-skimmer-field-node.md](docs/RUNBOOKS/secondary-skimmer-field-node.md)
(a 30-day secondary-skimmer field node behind an RBN Aggregator, MAN-96) ·
`manta <subcommand> --help` for every flag.

**Contributing:** [ARCHITECTURE.md](ARCHITECTURE.md) — the nine-crate
workspace, data flow, and the channelizer, decoder, validation and output
design · [docs/DECISIONS/](docs/DECISIONS/) — dated design decisions and
implementation pins · [wiki/INDEX.md](wiki/INDEX.md) — accumulated
gotchas · [CHANGELOG.md](CHANGELOG.md) — changes and the decoder-output
versioning rule.

**Algorithms and research:** [docs/SPEC-decode-core.md](docs/SPEC-decode-core.md) —
channelizer constants, noise-floor estimator, track state machine,
decoder equations, confidence formulas, determinism rules, golden
vectors, config-key table · [ROADMAP.md](ROADMAP.md) — milestones M0 to
M4 with acceptance criteria · `manta bench sensitivity` — the
recall/CER-vs-SNR curve on synthetic signals, regenerable from any build.

## Related projects

- [coppa](https://github.com/HagaleTechnologies/coppa): `manta` reuses its
  FFT and its AWGN / Watterson HF channel models for the DSP core and test
  harness.
- **dit**: `manta`'s decoder is the wideband, headless evolution of dit's
  single-channel CW engine.
- [cqdx](https://cqdx.app): `manta`'s JSON spot stream is designed as a
  first-class cqdx ingest source.

## Contributing

Open an issue or a pull request. Main moves only by PR, CI must be green, and
the golden-vector determinism tests are the bar every decoder change has to
clear. See [SECURITY.md](SECURITY.md) for vulnerability reporting.

When you report a problem, include the output of `manta --version`. It names
the exact commit and the compiled-in features. The Docker image is the
exception for now: its build has no git metadata, so its commit reads
`unknown`. A change that alters decoder
output adds a `### Decoder output` entry to [CHANGELOG.md](CHANGELOG.md).

## License

MIT OR Apache-2.0, at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this project shall be
dual licensed as above, without any additional terms or conditions.

## Keeping the prefix table current

A new prefix must appear in the country table before manta can spot it.
Download AD1C's `cty.dat`, set its path under `[spot]` in your config,
check the config, and restart the daemon:

```sh
sudo curl -fsSL -o /etc/manta/cty.dat https://www.country-files.com/cty/cty.dat
sudoedit /etc/manta/manta.toml
# Under [spot], set cty_path = "/etc/manta/cty.dat"
sudo manta config check --config /etc/manta/manta.toml
# If manta is installed as a systemd service:
sudo systemctl restart manta
```

Use `scp_path` or `--scp` for an updated known-callsign list downloaded
from https://www.supercheckpartial.com/MASTER.SCP. Both settings replace
only their own table and take effect on restart. A missing, unreadable,
non-UTF-8 or empty table stops startup; `config check` catches the same
errors before opening a receiver or binding a port. Its `spot:` line
shows the resolved paths, or `bundled` for a table built into manta.

Config-file paths are relative to that config's directory. Flag values
and `MANTA_SPOT_CTY_PATH` / `MANTA_SPOT_SCP_PATH` are relative to the
working directory; flags override environment values, which override
the file. `decode` accepts flags and config keys but ignores environment
variables and never prints an age warning.

For a systemd config copied into a credentials directory, use absolute
paths for `cty_path`, `scp_path`, `blocklist_path` and `notch_path`. On
macOS, keep the table readable by the service account (`_manta` when
using that account) and restart the launchd service. In Docker, mount
each table read-only at the configured container path and restart the
container. Mounting the config alone does not mount the tables it names.
