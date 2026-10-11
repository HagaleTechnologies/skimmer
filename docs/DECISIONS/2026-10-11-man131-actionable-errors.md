# 2026-10-11 — MAN-131: errors say what to check next

**Status:** Implemented (branch `MAN-131`). Found by the 2026-09-05 broad
review, lens 1 (#11/#17/#20/#25).

## Context

The ticket has three scenarios: a failing audio device should name the
device, the rate and (on macOS) the microphone-permission setting; a port
conflict should name the address and port; and `manta decode` without a
`<stem>.json` sidecar should warn and accept `--center-freq-hz`.
Reproduced on `main` at `9644a7f`:

| What | Result |
|---|---|
| A wall-clock-paced fake 48 kHz device (480 samples per 10 ms, 8192-sample ring, `CpalSource`'s semantics) through `AudioIqSource` and `listen` | `read #2 returned 0 at 13 ms`; `listen` returned `Err("audio source ended during startup calibration")` after 15 ms |
| `manta listen --device Discard --dial-freq-hz 14030000` (ALSA's null capture device) | exit **0**, no message, after 2.7–3.9 s |
| `--device NoSuchCard` on `run`/`listen`/`soak`/`doctor`/`check` | `no input device matching "NoSuchCard"`, exit 1 |
| `manta listen` on a host whose default device has no card | `query device default input config` / `The requested audio device is not available…` |
| a held telnet, JSON or metrics port | `binding the <listener> server (<addr key> = "127.0.0.1", <port key> = N)`, exit 1 |
| `manta decode` on a copy of `v1.wav` without `v1.json` | `freq_hz: 12349.9`, exit 0, nothing on stderr under `--json` (14 MHz too low) |

The ticket's sample message, `audio source ended during startup
calibration`, does not come from the device. coppa's `CpalSource::read`
returns whatever its callback ring holds right now, 0 when it is empty;
`AudioIqSource` forwarded that 0, and `IqSource`'s contract reads 0 as end
of stream. `listen` therefore stopped at the first empty ring: during
calibration with that error, and in steady state with a clean exit 0.
Rewording the message alone would have sent operators to their microphone
settings for a manta bug.

## Decisions

- **D1 — live audio reads wait for samples.** `AudioIqSource::from_device`
  builds a live source whose `read` polls the ring every 10 ms (the
  interval `manta check` already used) until at least one sample arrives,
  or fails after `DEVICE_STALL_TIMEOUT` = 5 s with the device, the rate and
  the hint. `IqSource`'s `0 = EOF` then holds for every source, so
  `listen`, `soak`, `doctor`, `check` and `run`'s `ReconnectingSource` all
  get correct behaviour with no engine change. 5 s rather than the ~10 s
  network-source bound (`hpsdr.rs`, `kiwi.rs`): local callbacks arrive
  every ~10 ms, and an operator should hear quickly. A read in flight is
  not interruptible, so Ctrl-C during a stall takes effect within 5 s. An
  empty request returns 0 at once. File-backed sources (`new`,
  `from_wav_file`) are unchanged, so replay stays byte-identical.
- **D2 — the device is named as `manta devices` spells it.** Names go
  through one helper, `manta_input::devices::quoted_name` (JSON quoting,
  every control character escaped), which `manta devices` now also uses,
  so an operator can copy a name from either. If cpal cannot describe the
  device, the error names what was requested (`matching "X"`) or
  `(system default)`.
- **D3 — each failure carries the next step that fits it.** Not found and
  no default: `(manta needs a 48000 Hz input) -- run `manta devices` to
  list the inputs --device can select`. Wrong native rate (now checked
  before the stream starts): `runs at N Hz, but manta needs 48000 Hz and
  does not resample -- set that input to 48000 Hz in the system's audio
  settings`, plus `(Audio MIDI Setup on macOS)` on macOS. Did not open and
  delivered no samples: the platform hint from `audio_input_hint()`. On
  macOS that names System Settings > Privacy & Security > Microphone, says
  to quit and reopen the app running manta, then to confirm with `manta
  check`; elsewhere it says to check the input is connected, unmuted and
  not held by another program. Every message is one line, `" -- "` before
  the next step, with no Debug formatting.
- **D4 — digital silence is a warning.** cpal cannot see macOS TCC: a
  denied microphone gives no callbacks (D1's stall error) or exact-zero
  buffers. When the first 2 s (`2 × 48000` samples) a live device delivers
  are all exact zeros, `AudioIqSource::with_silence_notice` calls its
  notice once, and `manta-cli` prints `warning: audio input "<name>" is
  delivering digital silence at 48000 Hz (every sample is exactly zero) --
  <hint>`. One non-zero sample disarms it. It is a warning, not an error:
  a muted input or a virtual device can legitimately deliver silence, and
  `doctor` still reports `NO_SIGNAL`. Every (re)open through
  `LiveSourceSpec::open` gets its own one-shot notice. Files never get
  one: silent WAV fixtures are routine.
- **D5 — the calibration string is unchanged.** `audio source ended during
  startup calibration` stays in `manta-engine`, and open PRs pin it. It is
  now reachable only from finite sources such as a WAV shorter than the
  calibration window.
- **D6 — `check` on a stalled device fails with the device message.** A
  live device that delivers nothing makes `manta check` exit 1 with D1's
  error after 5 s, instead of a `sampling_deadline_reached` report with
  zero samples after duration + 5 s. `check`'s `zero_read_is_transient`
  retry stays as defensive code; live audio no longer returns 0.
- **D7 — port conflicts were already met.** MAN-132 D8 wraps each bind in
  context naming the listener, its address key and its port. The
  regression test (`metrics_bind.rs`'s
  `every_failed_bind_names_its_listener_address_and_port`) now covers
  telnet and JSON as well as metrics. No message change.
- **D8 — `decode` warns without an RF centre, and takes
  `--center-freq-hz`.** Before decoding, an existing WAV whose sidecar is
  missing, or gives `center_freq_hz <= 0`, gets one stderr line: `warning:
  no sidecar <stem>.json next to <wav> -- reported frequencies are
  baseband offsets from the recording's centre, not absolute RF
  frequencies. Pass the centre frequency, e.g. --center-freq-hz 14000000.`
  `--center-freq-hz HZ` overrides the sidecar, with no warning, and is
  validated like `--dial-freq-hz` (finite and positive; clap exits 2
  otherwise). With the flag the sidecar is not read at all
  (`WavIqSource::open_with_center_freq_hz`), so a malformed or foreign
  `<stem>.json` cannot fail the decode. `decode --json` stdout is unchanged without the flag, and
  `--center-freq-hz 14000000` on a sidecar-less copy reproduces the 14 MHz
  sidecar decode byte for byte. A missing WAV still fails with just its
  `open WAV` error. Scope is `decode` only: not `oracle`, and not
  `input.center_freq_hz` from `--config`, which describes the live
  receiver's tuning — applying it to a recording would silently relabel
  captures made on another band. MAN-34 covers the live rig-audio path.
  `manta_input::read_sidecar` and `sidecar_path` are shared by
  `WavIqSource::open` and the warning, so they cannot disagree about the
  file.

## Consequences

- `run`/`listen` on a healthy sound card decode until stopped; they no
  longer end when the capture ring is momentarily empty.
- `run` keeps MAN-73's retry-forever behaviour for live sources: a device
  that stops delivering is reported (`source audio lost: audio input "…"
  delivered no samples in 5 s at 48000 Hz -- <hint>; reconnecting in 2s`)
  and reopened. `doctor` and `soak` are one-shot and exit 1 with the same
  text.
- Scripts that matched `no input device matching` should match `no audio
  input matching`. None exist in this repo.
- Decoder output is unchanged: no `### Decoder output` CHANGELOG entry.

## Follow-ups (not in this change)

- A real macOS denied-permission run and a healthy USB sound card past
  60 s are hardware evidence this container cannot produce.
- The coppa ring stays 8192 samples (~170 ms); overruns while `listen`
  processes its calibration block are MAN-56's open gap.
- `ReconnectingSource` returning `Ok(0)` on Ctrl-C during an outage (MAN-73
  validation note) is unchanged.

## References

- `crates/manta-input/src/audio.rs` — `from_device`, `read_real`,
  `audio_input_hint`, `with_silence_notice`, the message constructors.
- `crates/manta-input/src/devices.rs` — `quoted_name`.
- `crates/manta-input/src/lib.rs` — `read_sidecar`, `sidecar_path`,
  `WavIqSource::open_with_center_freq_hz`.
- `crates/manta-cli/src/main.rs` — `audio_silence_warning`,
  `recording_center_warning`, `parse_center_freq_hz`, the `Decode` handler.
- Tests: `crates/manta-cli/tests/live_audio_errors.rs`,
  `crates/manta-cli/tests/decode_sidecar.rs`,
  `crates/manta-cli/tests/metrics_bind.rs`.
- `docs/DECISIONS/2026-10-08-man132-metrics-loopback-bind.md` (D8),
  `docs/DECISIONS/2026-10-10-man125-source-diagnostics.md`,
  `docs/DECISIONS/2026-10-05-man73-source-reconnect.md`.
