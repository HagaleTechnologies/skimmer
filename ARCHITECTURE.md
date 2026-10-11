# manta — Architecture

Headless wideband CW skimmer: SDR IQ in, RBN-compatible spots out.

This document records the core design decisions. It is deliberately opinionated:
where a choice had to be made, it is made here, with the rationale. Sections
marked **(research-dependent)** are the only intentionally open questions.

## 1. System overview

```
                ┌─────────────────────────────────────────────────────────────┐
                │                         manta daemon                         │
                │                                                              │
 RTL-SDR ──┐    │  ┌──────────┐   ┌─────────────┐   ┌───────────────────────┐  │
 Airspy  ──┼─▶  │  │  input   │──▶│ channelizer │──▶│  detector             │  │
 SDRplay ──┘    │  │ (IQ src) │   │ (PFB, 1024  │   │  (noise floor, SNR    │  │
 (SoapySDR)     │  └──────────┘   │  channels)  │   │   gate, track mgmt)   │  │
 KiwiSDR ──────▶│       ▲         └─────────────┘   └──────────┬────────────┘  │
 (network)      │       │                                      │ active tracks │
 IQ/WAV file ──▶│       │ config                    ┌──────────▼────────────┐  │
 rig audio ────▶│       │                           │  decoder pool         │  │
 (cpal)         │       │                           │  (per-signal CW       │  │
                │       │                           │   decoders, N ≤ 1200) │  │
                │       │                           └──────────┬────────────┘  │
                │       │                                      │ decoded text  │
                │  ┌────┴─────┐   ┌─────────────┐   ┌──────────▼────────────┐  │
                │  │ metrics/ │◀──│ spot server │◀──│  validator            │  │
                │  │ tracing  │   │ telnet+JSON │   │  (callsign, CQ/DE,    │  │
                │  └──────────┘   └──────┬──────┘   │   dedupe, confidence) │  │
                │                        │          └───────────────────────┘  │
                └────────────────────────┼─────────────────────────────────────┘
                                         │
                        telnet :7300 (DX cluster protocol, RBN format)
                        tcp/ws :7301 (JSON Lines spot stream → cqdx)
```

Data flows one direction. Every stage is a bounded-queue actor on the tokio
runtime except the channelizer and decoders, which run on dedicated
compute threads fed by lock-free SPSC rings (`rtrb`, same as coppa-audio) —
IQ never touches the async runtime.

## 2. Workspace layout

Multi-crate workspace following coppa's conventions (edition 2021, MIT/Apache-2.0,
workspace-level dependency table, `criterion` benches, `proptest` where invariants
allow).

```
manta/
├── Cargo.toml                 # workspace
├── crates/
│   ├── manta-input          # IQ sources: SoapySDR, HPSDR/Hermes, KiwiSDR client, file, audio
│   ├── manta-dsp            # PFB channelizer, noise-floor estimation, envelope
│   ├── manta-decode         # CW keying state machine, timing, Morse decode
│   ├── manta-spot           # callsign validation, CQ/DE parse, dedupe, scoring
│   ├── manta-server         # telnet cluster server + JSON/WebSocket stream
│   ├── manta-engine         # orchestration: track lifecycle, decoder pool
│   ├── manta-testkit        # synthetic CW generator, golden-IQ harness
│   ├── manta-cli            # `manta` binary: daemon + subcommands
│   └── manta-soak-harness   # 24h soak measurement harness (ROADMAP M2 gate),
│                             # not shipped in the manta binary
```

Dependency graph (arrows = depends on):

```
manta-cli ──▶ manta-engine ──▶ manta-input ──▶ manta-dsp
        │            │        ├──▶ manta-dsp ──────▶ coppa-dsp
        │            │        ├──▶ manta-decode
        │            │        └──▶ manta-spot ──────▶ manta-decode
        └──────────────────▶ manta-server
manta-testkit ──▶ manta-dsp, manta-decode, coppa-channel
manta-soak-harness ──▶ manta-dsp, manta-input, manta-engine, manta-testkit
```

M1 added `manta-input → manta-dsp` (the shared Hilbert transformer, used
by both `AudioIqSource` and `manta-testkit`'s Watterson vector rendering)
and `manta-testkit → coppa-channel` (Watterson fading, see the M1
pinned-decisions doc).

### Reused from coppa vs. new

| Capability | Source |
|---|---|
| FFT (`FftProcessor`) | **reuse** `coppa-dsp::fft` |
| FIR design (PFB prototype) | **new** (`manta-dsp::proto`) — `coppa-dsp::filter` ships only `RrcFilter` (SPEC §10.1) |
| Envelope normalization | **new** — per-track fixed reference scale; `coppa-dsp::agc` not used in the decode path (SPEC §10.2) |
| Channel impairments for tests (AWGN, freq offset, fading, **Watterson HF**) | **reuse** `coppa-channel` |
| Audio-device input (single-channel mode) | **reuse** `coppa-audio` (cpal) — no automatic resampling; source must run natively at exactly 48000 Hz (M1 pinned decisions doc) |
| Real-to-analytic Hilbert conversion | **new** (`manta-dsp::hilbert`) — used by both live audio input and offline Watterson vector rendering |
| Polyphase filterbank channelizer | **new** (`manta-dsp`) — coppa has no channelizer |
| Order-statistic noise-floor estimator | **new** (`manta-dsp`) |
| CW keying/timing/Morse decode | **new** (`manta-decode`) — dit's algorithms, ported & headless |
| Callsign/spot validation | **new** (`manta-spot`) |
| DX cluster telnet protocol | **new** (`manta-server`) |

coppa crates are consumed as git dependencies (path deps during co-development in
this workspace-of-workspaces). If coppa publishes to crates.io first, switch to
versioned deps.

## 3. Input layer (`manta-input`)

One trait, five implementations:

```
trait IqSource: sample_rate(), center_freq(), read(&mut [Complex32]) -> …
```

- **SoapySDR** (`soapysdr` crate, feature-gated `soapy`): RTL-SDR (2.4 MS/s max,
  8-bit), Airspy HF+ (768 kS/s, the reference device), SDRplay. Runtime device
  selection by driver string. Feature-gating keeps the core buildable without the
  native SoapySDR library (CI, contributors without hardware).
- **OpenHPSDR/Hermes** (Protocol 1 "Metis" over UDP, feature-gated `hpsdr`):
  Hermes-Lite 2 and Pavel Demin's Red Pitaya and QMTech images. Pure UDP/std with
  no native-library dependency; the gate mirrors `soapy`. The wire facts are
  spike-pinned (`docs/DECISIONS/2026-09-02-hpsdr-hermes-protocol-spike.md`), not yet
  confirmed against live hardware.
- **KiwiSDR client**: the kiwisdr websocket IQ protocol (12 kHz IQ per channel) —
  narrow, but gives instant worldwide receiver access for development and lets
  low-budget nodes contribute spots.
- **File playback**: WAV (via `hound`, matching coppa) and raw interleaved
  `f32`/`i16` IQ with a small JSON sidecar for rate/center-freq. Drives the entire
  test strategy; the daemon must run identically from file and live SDR.
- **Audio passband** (via `coppa-audio`): 48 kHz real audio from a rig's RX audio,
  Hilbert-transformed to analytic. Degenerate ~3 kHz "wideband" mode; exists
  because it makes manta useful to people with zero SDR hardware, and it is the
  M1 bring-up path.

**Sample-rate assumptions.** Design center: 96–192 kS/s complex (covers any HF CW
band segment; CW allocations are ≤ 100 kHz wide). Supported ceiling: 768 kS/s
(Airspy HF+ full span). The channelizer parameterizes N to hold channel spacing
near 100 Hz regardless of input rate (§4). Multi-band via multiple daemon
instances, not one instance retuning — simpler, and SDRs are cheap.

**Variable-width capture** (issue #169): an optional decimation stage
(`manta-dsp::decimate::Decimator`, wrapped as
`manta-input::DecimatingSource`) sits between a live `IqSource` and the
channelizer.
Operators select a narrower effective capture rate via
`--capture-rate-hz`; the SDR still opens at its best native rate, and a
cascade of Kaiser-windowed halfband FIR decimate-by-2 stages narrows it
down before the channelizer ever sees it. Only exact power-of-two
factors are supported (no general resampling), and the resulting rate
must itself satisfy the channelizer's `fs/93.75` table constraint.
Motivated by live-hardware field evidence (2026-09-09) that a narrower
capture bandwidth can improve real-signal detection on some hardware.

All sources normalize to `Complex32` at the native rate into an `rtrb` ring;
input overruns are counted, surfaced as metrics, and never block the SDR thread.
This ring-overrun counting is still aspirational (`manta-engine::soak`'s module
doc tracks the blocker: `coppa-audio::CpalSource` doesn't expose its ring's
`overflow_count()` publicly). Distinct and already shipped (MAN-56): HPSDR's
wire-level UDP packet loss/malformed-datagram counters
(`manta_input::InputHealthCounters`, §8) reach the Prometheus `/metrics`
endpoint today — a different layer (lost/rejected datagrams before demux, not
ring backpressure after it), not a partial implementation of the ring gauge
above.

**Reconnect (MAN-73).** A live source's `IqSource::read()` can fail -- a
stalled Kiwi socket, an HPSDR stall-escalation, a dropped audio device.
`manta-cli` wraps every such source (not file replay) in
`ReconnectingSource`, which reopens it with `manta-server::backoff`'s
1s-60s policy instead of letting the error reach `listen()` and end the
process. Samples lost to the outage are reported once via the trait's
`take_discontinuity()` method; `manta_engine::listen` responds by closing
the current track segment and starting a fresh one with the sample clock
advanced by the gap, so spot timestamps stay wall-clock-true with no
audio spliced across the gap and no zero-fill (see
`docs/DECISIONS/2026-10-05-man73-source-reconnect.md` for why zero-fill
was rejected). File replay is exempt: its errors and EOF must reach
`listen()` unchanged for byte-identical replay.

## 4. Channelizer (`manta-dsp`)

**Implemented** as of M2 sub-project 1 (`manta-dsp::channelizer`) -- the
design below is now built, not just decided.

**Decision: 4×-oversampled polyphase filterbank (PFB), ~100 Hz channel spacing,
detection on channel powers, decoders attached only to active channels.**

- N = input_rate / ~93.75 Hz, rounded to a power of two: N=1024 at 96 kS/s,
  N=2048 at 192 kS/s, N=8192 at 768 kS/s. Channel spacing = rate/N ≈ 94 Hz.
- Prototype lowpass: Kaiser-designed FIR (new code, `manta-dsp::proto` — see
  SPEC §1.2), 8 taps/branch,
  passband ~140 Hz — each channel fully contains a CW signal up to ~45 WPM
  (occupied BW ≈ 4·WPM Hz ≈ 180 Hz at 45 WPM spans ≤ 2 channels; the decoder reads
  the peak channel, and the 50%+ spectral overlap between adjacent channels means
  no signal is lost straddling an edge).
- **4× oversampled outputs** (hop = N/4): per-channel output rate ≈ 375 Hz. At
  40 WPM a dit is 30 ms ≈ 11 samples — comfortably enough for envelope timing.
  2× (187 Hz, 5.6 samples/dit) was rejected as too marginal for QSB'd fast CW.
- Implementation: polyphase FIR commutator + one N-point FFT per hop
  (`coppa-dsp::fft::FftProcessor`). Frequency-domain output magnitude² feeds the
  detector directly — the PFB *is* the spectrum analyzer; no separate FFT path.

**Detector / track manager.** Per-channel noise floor by order statistics
(median of channel power over a sliding ~10 s window — median, not mean, so CW
keying doesn't inflate its own floor). A channel goes *active* when smoothed power
exceeds floor + threshold (default 6 dB) with hysteresis (3 dB drop + 5 s hang to
survive QSB and inter-word gaps). Active channel ⇒ a **track** (center channel ±1
neighbor, combined by max-power selection) ⇒ a decoder is leased from the pool.
Track cap (default 1200, MAN-166: raised from 500, which was never
stress-tested against real contest-band signal density and was pinned at
its ceiling for the entire duration of a real 15-minute recording,
`docs/DECISIONS/2026-09-09-man166-confirm-hops-and-track-cap.md`) with
lowest-SNR eviction; evictions are counted and reported (no silent
coverage loss).

**CPU budget** (the reason this whole design is viable):

| Stage | Cost at 192 kS/s | Notes |
|---|---|---|
| PFB FIR (8 taps/branch, complex) | ~12 MFLOP/s | 192k samples × 8 CMACs |
| FFT (2048-pt, 375/s) | ~42 MFLOP/s | 5·N·log₂N per FFT |
| Detection (power, medians) | ~5 MFLOP/s | incremental order statistics |
| 300 active decoders @ 375 Hz | ~10 MFLOP/s | envelope + state machine is cheap |
| **Total** | **< 100 MFLOP/s** | **≪ 1 core**; a Pi 4 core does ~5 GFLOP/s |

Even at 768 kS/s the pipeline stays under half a core; the machine's job is I/O,
not math. This budget is enforced by `criterion` benches in CI (M2 acceptance).

## 5. Per-channel decoder (`manta-decode`)

The wideband, headless port of dit's proven single-channel chain. Classical
first; ML is a fusion stage later (M4), exactly as dit evolved.

Per track, operating on the ~375 Hz complex channel stream:

1. **Envelope**: |x| → per-track fixed reference scale (SPEC §3.1; coppa's
   block AGC is not used) → smoothed magnitude.
   (A separate tone-finder stage is unnecessary here — the PFB already did the
   frequency selection.)
2. **Keying detection**: dual-rail noise/signal EMA estimators → key-down/
   key-up decisions from an additive band about the rails' linear-amplitude
   midpoint (SPEC §3.2/§3.3; superseding the geometric mean this section
   originally described — a geometric-mean threshold sits closer to the
   noise rail as apparent keying depth grows, biasing every measured mark
   long, worse at higher SNR and near a channel edge; see
   `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`) → debounce.
3. **Speed tracking**: online 2-means clustering of mark durations into
   {dit, dah}; WPM = 1200/dit_estimate_ms (a symmetric mark/gap period
   estimate, SPEC §4.1a — not dit_ms directly, which stays uncorrected for
   the classification/likelihood consumers that want it), tracked with EMA.
   Handles 10–40+ WPM and drift; Farnsworth spacing tolerated by decoupling
   inter-element and inter-word gap
   thresholds (dit's speed-detector lesson).
4. **Element→character decode**: marks/spaces classified against the tracked
   timing model with per-element likelihoods, then a **beam search (width 4) over
   the Morse code tree** — small-Viterbi rather than hard thresholding, so a
   marginal dit/dah keeps both hypotheses alive until character boundary. Emits
   characters with confidence.
5. **(M4, research-dependent) ML decoder**: small CTC model on the channel
   envelope, fused with the classical decoder by adaptive confidence weighting —
   a direct port of dit's fusion-engine design (sliding-window accuracy
   tracking, EMA-smoothed weights, weight floor). Training corpus comes from
   `manta-testkit` synthesis + RBN-validated on-air recordings. The classical
   decoder must ship first and defines the accuracy baseline the ML stage has to
   beat under QRM/QSB (measured, not assumed).

Decoder output: timestamped character stream + WPM + SNR + confidence per track.

## 6. Spot validation (`manta-spot`)

Decoded text is noisy; validation is what makes spots trustworthy. Pipeline per
track, over a rolling text window:

No spot is ever emitted before a track's first `TrackMeta` event (SPEC §5, 1 Hz
cadence) — until then `freq_hz`/`snr_db` hold bogus `0.0` defaults, and a spot
carrying them would poison both the emitted record and dedupe's frequency
bucket. The old ≥2-repetition gate hid this by construction (reaching two
repetitions takes long enough that real telemetry always arrived first); the
BEACON/allowlist exemptions below removed that incidental protection, so it is
now an explicit invariant checked before any candidate is evaluated (MAN-28).
A candidate held back by this gate is retried the moment `TrackMeta` arrives
(not left waiting on a `WordBoundary` that a short, already-finished
transmission may never produce again).

1. **CQ/DE context parse**: regex-level scan for `CQ <call>`, `CQ TEST <call>`,
   contest framing `CQ <contest> <call>` (an enumerated filler set, e.g.
   `CQ WPX`) and a bare `TEST <call>` (MAN-104; a bare `TEST` between two
   different callsigns, or after a sign-off such as `TU`, is ambiguous and
   yields no candidate),
   `DE <call>`, `<call> UP`, beacon patterns (`V V V <call>`, and `<call> T`
   for NCDXF-style power-step beacons the decoder can't resolve past a
   single trailing dash, MAN-37 — suppressed whenever a bare `CQ`/`TEST`/`DE`
   token appears anywhere in the window at all (the token must be a
   complete decoded word, not a substring glued to punctuation inside
   one), a deliberately coarse guard against mistagging an ordinary,
   unrecognized CQ/DE call as Beacon — each occurrence it actually costs
   is counted once, as `SuppressionCounts::power_step_guard` (§8); an
   occurrence already evaluated as a Beacon before the guard appeared is
   not a loss and is not counted). Context
   determines spot type (CQ / DE / BEACON) — RBN spots carry this flag.
2. **Callsign plausibility**: structural grammar (prefix-digit-suffix, portable
   designators `/P /QRP /3`), which also rejects, by exact match, a fixed
   list of non-callsign CW conventions (`5NN`, `599`, `TEST`, `TU`, `QRZ`,
   `AGN`, `K`, `KN`, …; MAN-105), regardless of cty.dat, then prefix
   lookup against **cty.dat**.
   A call with an unallocated prefix is rejected. Operators can replace the
   bundled table with `--cty` / `[spot] cty_path`. Live commands warn when
   the built-in copy is more than 180 days old (MAN-79). `cty.dat` is
   also joined, on that same primary-prefix field, against a small vendored
   ADIF DXCC entity-number table (`data/dxcc.tsv`, MAN-136) — refreshed
   together, see `crates/manta-spot/data/SOURCES.md` — which is what lets
   `manta-server`'s JSON stream populate `dxDxcc`/`deDxcc` (§7).
3. **SCP cross-check** (optional, default on if file present): membership in
   `master.scp` (contest super-check-partial list) *raises* confidence; absence
   only lowers it (new/rare calls must still spot, not just well-known ones).
   `--scp` / `[spot] scp_path` supplies a replacement known-callsign list.
4. **Repetition requirement**: a callsign must decode ≥ 2 times within 90 s
   before first spot (CW ops repeat their calls; single decodes are
   overwhelmingly garble). **Deviates from "the same track" (MAN-166,
   `docs/DECISIONS/2026-09-09-man166-confirm-hops-and-track-cap.md`)**: a
   real signal's `track_id` changes across a close+reopen, so repetition is
   tracked per frequency instead, with a minimum-gap check across
   *different* track_ids to still reject two tracks concurrently decoding
   one real transmission as a false second confirmation — see
   `crates/manta-spot/src/gate.rs`. Confidence = f(decoder confidence,
   repetitions, SNR, SCP/cty hits). **Exemption**: messages already
   type-tagged `BEACON` by step 1's context parse skip this gate entirely
   — NCDXF-style beacons ID once per power-step cycle and legitimately
   won't repeat within the window (MAN-28). **"Distinct" requires separate
   messages** (MAN-100): two decodes of the same text count as one
   repetition, not two, unless they clear `MIN_MESSAGE_WORD_GAP` (3)
   decoded words apart **or** `MIN_MESSAGE_TIME_GAP_SECONDS` (60 s) of
   `sample_ts` apart (MAN-100 remediation C2) — SPEC's own default payload
   template repeats the callsign back-to-back within a single transmission
   (`CQ CQ DE <CALL> <CALL> K`), and without the word-gap half of this rule
   that one message's fading-corrupted double utterance alone could
   satisfy the gate; without the time-gap half, a short ID (e.g.
   "DE `<CALL>`") puts even two genuinely separate transmissions only 2
   words apart, which a word-gap-only rule cannot tell apart from one
   message's double utterance.
4b. **Cross-candidate variant arbitration** (MAN-100): before a candidate
   spots, it's checked against every other decoded, spottable-shaped word
   (one that itself passes steps 1's grammar/cty check) observed on the same
   track within the same 90 s window. It's withheld if a confusable,
   better-supported rival exists — confusable meaning a substring/superstring
   relationship or a shared ≥ 3-character prefix at edit distance ≤ 2;
   better-supported meaning strictly more message-distinct repetitions (ties
   broken by summed per-occurrence confidence), or the candidate being a
   strict prefix of a rival that has been observed at all (≥ 1 repetition
   of its own — shape decides once a rival exists, regardless of how
   little support it has; MAN-100 remediation round 3 reverted an earlier
   attempt to also require the rival to clear the same ≥ 2-rep floor a
   spottable candidate must, since that excluded the ticket's own
   measured "W6JQ"/"W6JQA" case) — and, symmetrically, a rival that is itself
   a strict prefix of the candidate never wins this comparison regardless of
   its own repetition count (MAN-100 remediation C1: shape decides a
   prefix-containment pair in both directions, not just when arbitrating
   the shorter form). This closes the gap that let a track spot both a
   real callsign and a fading-truncated fragment of it as if they were two
   different stations — measured on a 50-signal CCIR-poor pileup at an 18%
   busted-spot rate among distinct spotted calls, none of which `c_call`
   alone could distinguish (bogus and genuine confidence ranges overlapped
   completely). Purely subtractive: this step can only withhold a spot the
   rest of the pipeline would have emitted, never produce one, so it can
   never itself cause a false spot. Never fires against an
   operator-allowlisted callsign, one present in the bundled `master.scp`,
   or a `SpotType::Beacon` candidate (MAN-100 remediation C3) — the beacon
   exemption mirrors step 4's own repetition-gate exemption immediately
   above: a once-per-cycle beacon's rep count is structurally low, so a
   confusable rival's fading-corrupted repeat could otherwise outrank and
   permanently suppress the genuine beacon on rep count alone. All three
   exemptions trade toward recall on exactly the population RBN cares
   about, at the cost (measured as zero on the available multi-signal
   test scenes, for the allowlist/SCP pair) of occasionally letting a
   truncation or confusable variant of an exempt call through unarbitrated.
5. **Dedupe/aggregation**: key = (callsign, freq bucket ±0.3 kHz); suppress
   re-spots for 10 min unless SNR improves ≥ 6 dB or type changes. Emitted spot
   carries freq (from PFB bin + track centroid, ~10 Hz absolute accuracy), SNR,
   WPM, type, confidence.

**Operator allowlist (Watch List)**: a callsign the operator explicitly lists
bypasses step 1's context-parse requirement too — not just steps 2
(grammar/cty) and 4 (repetition) — since a listed callsign with no
recognized CQ/DE/UP/beacon framing (tagged type `Unknown`) is exactly the
primary real-world case: an NCDXF beacon transmits its callsign followed
by power-step dashes, no framing words at all. Evaluated independently of
context parsing, not as a lower-priority fallback: a stale, already-
processed context match elsewhere in the rolling word window never blocks
discovery of a different, freshly-allowlisted word (`Validator::candidates`
gathers both per event). An immediate `Unknown`-typed spot is not final: if
a trailing word later completes a real context pattern for the same word
(e.g. `<call> UP` -> `De`), that reclassification is emitted as a second
spot via dedupe's existing type-changed override (step 5) -- an
already-processed word is not permanently locked to its first type.
Reclassification requires a genuinely younger word: `manta-spot::context`
returns not just a candidate/type but the byte span of every word that
determined it, which the validator maps back to word identities and their
insertion order; a later classification is only accepted when it involves
a word strictly newer than any that produced the previous one. This is
what makes reclassification promotion-only in practice -- a word is never
downgraded (to `Unknown`, or between two real context types) just because
an older framing word ages out of the rolling window, since aging out
never introduces a *newer* word, only removes an old one. Legacy
precedent: CW Skimmer's Watch List (Aggregator manual Appendix A2), which
exists specifically for calls that wouldn't otherwise pass automatic
validation (MAN-28). Dedupe (step 5) still applies.

## 7. Output layer (`manta-server`)

- **Telnet DX cluster server** (default :7300): on connect, sends a CW-Skimmer-
  shaped greeting banner (software name/version, operator name/callsign/QTH/
  grid, then `Please enter your callsign: `), validates the login as a
  plausible callsign shape (not authentication — see Exposure policy below),
  then emits spots in RBN's fixed-column AK1A layout —
  `DX de W3XYZ-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      0312Z`
  (frequency to 0.01 kHz ending at column 24, a 15-wide callsign column,
  time at column 71 — MAN-88, measured against a live
  `telnet.reversebeacon.net:7000` capture). `[server].line_format =
  "skimmer"` selects the CW-Skimmer-native variant (no mode column) for
  operators running manta behind W3OA's Aggregator, which expects CW
  Skimmer's own layout rather than the RBN relay's.
  Read-mostly protocol; enough command grammar (`sh/dx`, filters, `SKIMMER/
  SETT`, `BYE`, `sh/version`) for common clients — and RBN's own Aggregator —
  not to choke. An unrecognised or malformed command gets a fixed
  `Unknown command` reply, and `sh/version` gets `manta <version>`.
  Valid `sh/dx` queries with no results send no reply (no error, header or
  trailer).
  `sh/dx` follows Aggregator's documented forms: `sh/dx N` is a count and
  `sh/dx Nm` a minutes window, never a band. Either can take a ` CW`/` RTTY`
  suffix, and the manta extension `BAND <band>` selects a band. Every query
  reaches back at most the fifty retained spots. See
  `docs/DECISIONS/2026-10-10-man92-telnet-commands.md`.
  `SKIMMER/SETT` replies with validation level and the live decodable
  passband (`SETT: vlNormal 14000.0-14070.0`); Aggregator will not forward
  spots from a source that never answers it (Aggregator manual v6.0 §9.2).
  This is the RBN/aggregator compatibility surface — see
  `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md` for the
  exact wire format and its primary sources. The
  SNR field is quoted in the 500 Hz reference bandwidth RBN/CW Skimmer use
  (MAN-102 / decision D3), converted from the decoder's native 2500 Hz
  measurement at render time — see `docs/SPEC-decode-core.md` §2.3.
  Telnet option negotiation (RFC 854 IAC) is stripped from the client's byte
  stream and refused — every option, always — so that clients which negotiate
  on connect can log in; manta implements no telnet options. See
  `docs/DECISIONS/2026-09-07-man87-telnet-iac-policy.md`.
- **JSON Lines stream** (TCP and WebSocket, :7301): full-fidelity spot objects
  (adds confidence, track id, decoder text context). This is the cqdx ingest
  surface; schema published in `dispensa` as a JSON Schema contract alongside the
  existing ecosystem contracts. Every spot carries a non-null, real `dxDxcc`/
  `deDxcc` (an ADIF DXCC entity number, MAN-136) whenever the callsign
  resolves against `cty.dat`; when it doesn't, `dxDxcc`/`dxContinent`/
  `dxCqZone` (and their `de*` counterparts) carry named, out-of-domain
  `UNKNOWN_*` sentinels rather than `null` or a fabricated-looking value —
  see `docs/DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md`.
  Its `snr` field keeps the native 2500 Hz measurement (no 500 Hz conversion)
  alongside an explicit `snrRefHz` field naming that bandwidth, so a consumer
  of either surface never has to guess which convention it's reading
  (MAN-102 / decision D3). Every spot's `decoderVersion` is
  `manta-<version>+<commit>` (MAN-83): the commit is SemVer build metadata
  naming the exact binary, and the part before `+` is the release the
  decoder-output rule in `CHANGELOG.md` governs. See
  `docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md`.
- Both servers are thin fan-out consumers of one broadcast channel; slow clients
  are disconnected, never back-pressure the pipeline. At shutdown each
  client's queued backlog is drained on a best-effort basis bounded by a
  per-client deadline (`tasks::CLIENT_DRAIN_DEADLINE`) that starts once the
  handler's shutdown branch actually runs; anything the deadline abandons is
  counted in `manta_spots_dropped_write_failed_total`, never silently
  truncated (§8). A handler already mid-write when shutdown fires still has
  to finish that one write first — bounded by its own `WRITE_TIMEOUT` — before
  it can even reach the drain branch; telnet's `sh/dx` history replay
  re-checks the shutdown signal before every history entry and, the moment
  it's observed, defers to the loop's own drain branch to deliver the live
  backlog with that branch's full unused budget rather than abandoning it,
  so its worst case matches the live-write arm's rather than scaling with
  replay depth (MAN-45 remediate, round-16 P1; round 17, CR-2/CR-3). The
  registry-wide `SHUTDOWN_DRAIN_DEADLINE` (`manta-cli`) is sized to outlive
  that combined true worst case — `2 * telnet::WRITE_TIMEOUT +
  CLIENT_DRAIN_DEADLINE`, not `CLIENT_DRAIN_DEADLINE` alone — see that
  constant's own doc comment for the full accounting. Telnet's pre-login
  handshake (prompt/read/banner, before a client ever reaches that loop) is
  excluded from this worst case because it is itself shutdown-aware: each
  step races `shutdown.changed()` and bails out, counting its backlog,
  rather than running its own (up to 50s combined) timeouts to completion
  first (round 17, CR-1).
- **Exposure policy (normative, not just observed behavior):** both servers are
  designed to be internet-reachable with no client authentication, matching the
  DX cluster/RBN ecosystem's own long-standing convention (CW Skimmer, SkimSrv,
  and Aggregator have the identical property) — this is a deliberate compatibility
  choice, not an oversight, and manta-specific client auth would make it
  incompatible with the clients it exists to interoperate with. See
  `docs/DECISIONS/2026-09-02-man23-threat-model.md` findings 10/20 for the full
  threat-model rationale. The metrics HTTP endpoint (§8) is NOT part of this
  compatibility contract — it's operational tooling, not an RBN-facing surface —
  so it binds loopback by default: `[server].metrics_bind_addr`, `127.0.0.1`,
  independent of the telnet/JSON `bind_addr` (MAN-132,
  `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D14). Widen it only
  deliberately, per `docs/RUNBOOKS/network-exposure.md`.

## 8. Configuration & observability

- **Single TOML config** (MAN-261,
  `docs/DECISIONS/2026-10-06-man261-config-surface.md`): one file with six
  tables — `[server]` (station callsign, which may carry an RBN `-N`
  per-band SSID per
  `docs/DECISIONS/2026-09-07-man-89-station-callsign-ssid-grammar.md`;
  bind address; ports),
  `[[rbn_uplink]]`, `[input]` (source type and its keys, dial frequency,
  capture rate, ppm correction), `[spot]` (watch list, blocklist and notch
  files, and `cty.dat`/`master.scp` overrides), `[detector]` (thresholds, timers, track cap) and `[decode]` —
  read by `run`, `soak`, `doctor` and `calibrate` (`--config`, else
  `MANTA_CONFIG`) and
  by `decode`/`oracle`. Precedence is flag, then `MANTA_<TABLE>_<KEY>`
  environment variable, then file, then default; `decode` and `oracle`
  never read the environment. Unknown tables, keys and `MANTA_*` variables
  are errors. `docs/SPEC-decode-core.md` §9 is the key table. Not yet
  configurable: a band plan (CW segment limits) and the
  compile-time constants SPEC §9 marks `not configurable yet`.
  `manta config check` runs `run`'s config pipeline up to its first source
  I/O (load, environment overlay, source resolution, blocklist/notch
  and cty/scp reads), then prints a per-table summary of the resolved settings without
  opening the receiver or binding a port; `manta config init` writes a
  scaffold with every key commented out at its default, pinned to the code
  defaults and the loader's key list by tests (MAN-76,
  `docs/DECISIONS/2026-10-07-man76-config-check-init.md`).
  `manta calibrate` resolves `[input]` the way `doctor` does, measures the
  receiver's frequency error against a known carrier
  (`manta-dsp::carrier` estimates it, `manta-engine::calibrate` picks the
  reference and gates the result), and after confirmation edits only
  `input.freq_correction_ppm` in place in the file it read, keeping every
  other byte, the file's mode and owner (MAN-127,
  `docs/DECISIONS/2026-10-11-man127-calibrate-command.md`).
- **`tracing` + `tracing-subscriber` with `EnvFilter`, implemented for
  `manta-server`'s three listeners (telnet, JSON/WS, metrics)** — landed
  2026-09-03 (MAN-59, `docs/DECISIONS/2026-09-03-man59-connection-audit-logging.md`):
  connect/login/disconnect events, per-IP quota and rate-limit rejections,
  `bounded_io` read rejections, malformed-WS-frame disconnects, and
  rejected metrics-endpoint requests are all logged (plain `fmt` output,
  `RUST_LOG`-controlled, default `info`) to give an operator a durable
  record to reconstruct an abuse incident after the fact. **The daemon
  also logs its own liveness** — landed 2026-09-07 (MAN-122,
  `docs/DECISIONS/2026-09-07-man122-operator-liveness-logging.md`): one
  startup banner (version, source, sample rate, dial frequency, station
  callsign, real bound telnet/JSON/metrics addresses) logged once every
  bind succeeds and before any listener task is spawned, plus a
  rate-limited periodic status line (`manta_server::status`,
  `status_interval_secs` in `[server]`, default 60 s, `0` disables) naming
  pipeline state, active track count, spots/min, connected client count,
  and uplink connection state. The banner says `listening:`, not `ready:`
  (review round 2): bound sockets are not evidence anything will decode,
  since a replay shorter than the two-second calibration window or a live
  source that fails its first reads exits with the pipeline never having
  started. Readiness is a second, strictly later line
  (`manta <ver> ready: decoding source=...`) emitted from the decode
  loop's first processed batch. The status line's `pipeline=` field
  (`starting`/`decoding`/`stalled`) is derived from a per-batch progress
  counter, so a wedged decode loop — a blocked `IqSource::read`, say —
  reads as `stalled` instead of republishing a frozen `tracks=N`
  indefinitely. Still aspirational: `manta-input`'s and
  `manta-engine`'s own internals carry no logging of their own yet
  (decode-pipeline internals, not the network-facing surface MAN-59
  scoped to, nor the daemon-lifecycle surface MAN-122 scoped to).
  **`manta status` implemented** (MAN-44,
  `docs/DECISIONS/2026-09-04-man44-uplink-status-surface.md`): reads a
  JSON `StatusDoc` (`crates/manta-server/src/status_doc.rs`) served on
  `GET /status` by the same metrics listener that serves `GET /metrics`
  and `GET /healthz` — deliberately NOT the local-control-socket design
  this section previously sketched; the ADR records why a Unix-only
  control socket was evaluated and not taken. Reports daemon uptime,
  spot/client counts, and — the ticket's actual scope — **per-target RBN
  uplink health**: each configured `[[rbn_uplink]]` target's own
  connected/sent/suppressed/reconnect counts (MAN-128's per-target
  registry) plus a derived `connected`/`flapping`/`down`/`disabled`
  verdict from a windowed reconnect rate
  (`RECONNECT_WINDOW`/`FLAPPING_RECONNECTS` in `metrics.rs`), so a stuck
  reconnect loop reads as unhealthy even while technically connected at
  the instant it's checked. Exit code doubles as a cron/Nagios check.
  Unlike `/healthz`, which deliberately ignores the uplink (MAN-128 D10),
  `/status` is about the uplink. Inherits `/metrics`'s unauthenticated,
  `0.0.0.0`-by-default exposure posture
  (`docs/RUNBOOKS/network-exposure.md`). Prometheus text
  endpoint (compiled in unconditionally, no feature flag — the "(feature
  `metrics`)" phrasing in older revisions of this doc was stale, no Cargo
  `metrics` feature has ever existed; the endpoint is served whenever
  the resolved config has a `[server]` table — `--server-config` is
  MAN-77's deprecated alias of `--config`): input overruns, active tracks, evictions, decode rate,
  spots/min, per-stage queue depths, spot confidence histogram — still
  aspirational for several of these fields; the currently-implemented
  subset is `manta_spots_total`, `manta_spots_dropped_lagged_total`,
  `manta_spots_suppressed_by_filter_total`,
  `manta_spots_dropped_write_failed_total`,
  `manta_spots_dropped_shutdown_total` (backlog abandoned because the
  daemon shut down while a client was still in its PRE-LOGIN/handshake
  phase — telnet's login prompt/read/banner and the JSON stream's
  WS-detection peek and WS-accept, the only sites that charge it — with no
  socket write having failed or timed out, exactly as the Prometheus HELP
  text says. It does NOT mean no write was attempted: the telnet
  login-read and banner branches are reached only after the `login: `
  prompt was already written successfully (`telnet.rs`), and the WS-accept
  branch can fire after `accept_async_with_config` has already put part of
  the 101 response on the wire. What it does mean is that the client never
  got past its handshake, so none of its backlog had been offered to a
  write yet. It is NOT the
  graceful-drain series: a per-client drain loop that exhausts
  `tasks::CLIENT_DRAIN_DEADLINE` records whatever it abandons on
  `manta_spots_dropped_write_failed_total` (§7), so that is the counter
  to watch for drain-deadline loss),
  `manta_spots_replay_abandoned_total` (`sh/dx` history entries never
  replayed because the replay write failed or shutdown intervened — kept
  out of the write-failure counter because a replay entry was already
  counted once in `manta_spots_total`),
  `manta_spots_unresolved_geography_total` (MAN-136/MAN-45 — a spot that
  went out carrying an `UNKNOWN_*` sentinel on either side, i.e. its dx or
  de callsign didn't resolve against `cty.dat`, *or* it resolved but its
  entity has no row in the vendored `dxcc.tsv`, *or* it carries a `/MM`
  or `/AM` designator that places it outside any DXCC entity),
  `manta_active_tracks` (MAN-45/MAN-122, below), per-protocol
  client-connected gauges, `manta_source_health`, the uplink counters,
  (MAN-44) `manta_uplink_target_recent_reconnects{target}`, and
  (MAN-56, landed 2026-09-04) `manta_input_dropped_packets_total`/
  `manta_input_gaps_detected_total`/`manta_input_malformed_packets_total`
  (`crates/manta-server/src/metrics.rs`). **MAN-128 (landed 2026-10-05)
  added**: `manta_spots_by_band_total{band,type}` (a new family alongside
  the unchanged `manta_spots_total` rollup — the two are incremented
  together and the labeled family sums to the aggregate);
  `manta_decode_latency_seconds` (a real Prometheus histogram: wall-clock
  time per steady-state input chunk, excluding the source-read wait and
  calibration — its `_count` rate is spots-pipeline throughput, closing
  the "decode rate" gap noted below); `manta_start_time_seconds`/
  `manta_uptime_seconds`/`manta_build_info{version,git_sha,features}`
  (process uptime and build metadata — `git_sha` is `"unknown"` in a
  Docker build, since `.dockerignore` excludes `.git`);
  `manta_uplink_target_*{target="host:port"}` (per-target labels for every
  uplink counter, derived-sum aggregates preserved under their old
  unlabeled names); and `manta_healthy`/`manta_listener_up{listener}`
  (the same verdict `/healthz`, below, reports). What's still genuinely
  missing: per-stage queue depths, spots/min, spot-confidence histogram,
  and **ring**-overrun counting for live audio (§3 — blocked on
  a `coppa-audio` API addition, `manta-engine::soak`'s documented
  deviation, a different gap from MAN-56's wire-level packet counters).
  The three `manta_input_*` series are published only for sources that
  actually count wire-level packet loss (HPSDR and KiwiSDR — MAN-128
  generalized MAN-56's gap-stat wiring to KiwiSDR's SND `seq` field;
  soapy/audio report none) and are **absent**, not a frozen zero, for
  every other source — "absent means not measured", so an operator never
  reads a placeholder as live data; the same distinction that motivated
  the `manta_active_tracks` caveat before it was populated.
- **Source-outage counters (MAN-96)**: `manta_source_outages_total{source}`
  (healthy → unhealthy edges) and `manta_source_down_seconds_total{source}`
  (seconds in outages that have ended), counted in
  `Metrics::set_source_health` and rendered at 0 for every registered
  source, so a source drop shorter than a scrape interval still shows. An
  outage in progress shows only as `manta_source_health == 0`. The 30-day
  field-node ledger reads both (`docs/RUNBOOKS/secondary-skimmer-field-node.md`).
- **`GET /healthz` (MAN-128)**: shares the metrics listener and its
  `metrics_bind_addr`.
  Returns `200 OK`/body `ok\n...` only while every registered source is
  healthy, every registered listener (telnet/JSON/metrics) is up, and the
  decode loop has made progress within the last 10 s (or was never armed —
  library use only; the daemon always arms it). Otherwise `503 Service
  Unavailable`/body `unhealthy\n...`, with one line per failing check so an
  operator can see why. The RBN uplink is deliberately **excluded** from
  this verdict — an uplink outage must not make an orchestrator restart a
  node that is decoding fine; uplink health is visible per target on
  `/metrics` only. `manta_healthy`/`manta_listener_up` on `/metrics` are
  computed by the exact same evaluation, so a Prometheus-only operator and
  a liveness probe can never disagree.
  **`manta_active_tracks` is now populated** (MAN-45, corrected
  2026-09-04; its *source* corrected again 2026-09-07 by MAN-122). It had
  been served-but-frozen at a constant `0` since 2026-09-03 because
  `manta_engine::listen()` exposed no hook for it. Two observers now carry
  the count out of the decode loop, both fed from the same number:
  `manta_engine::listen_with_observers` stores it into a shared handle
  (`ListenObservers`), which the daemon's server runtime polls into
  `Metrics` every `ACTIVE_TRACKS_POLL_INTERVAL` (250 ms); and the same
  function hands it synchronously to an `on_tracks` callback after every
  processed batch — repeats included, since that call is also the daemon's
  decode-progress heartbeat (MAN-122 review round 2), which a polled gauge
  value alone cannot express. Plain `listen()` (every other caller —
  `soak()`, the CPU-budget bench, both integration tests) registers
  neither and pays nothing for this.
  The number itself comes from `TrackManager`'s own lifecycle, not from
  the decode event stream: `TrackManager::decoding_track_count()` reports
  how many tracks are currently promoted and holding a leased decoder
  (`Active` or `Hang`). So `manta_active_tracks` and the status line's
  `tracks=` field both report a real, moving count instead of the previous
  permanent `0`. An event-derived count was tried first and rejected in
  review: a track `TrackManager` has promoted but whose demodulator has
  not latched emits no events at all — `TrackDecoder` withholds
  `TrackMeta` until `snr_2500_db()` is `Some` — and such a track can stay
  ACTIVE until the ~30 s `gc_hops` silent GC, so a weak or unmodulated
  signal that real decoders are working on would have reported `tracks=0`.
  The gauge is driven back to `0` at end of stream and on a mid-stream
  read failure, so it doesn't stay stuck at the last live value after EOF
  or a fatal source error. A reconnectable source's disconnect never
  reaches `listen()` as a read failure (MAN-73, below): its unhealthy
  transition zeroes the shared handle instead, which the poller publishes
  within one `ACTIVE_TRACKS_POLL_INTERVAL`. Note this is deliberately *not*
  `TrackManager::active_track_count()` (what MAN-45 first published here),
  which also counts unconfirmed CANDIDATEs — noise-blip rise crossings
  that lease no decoder and would inflate an operator-facing "is it
  decoding?" reading — and which keeps its own meaning for `soak_metrics`.
  **`manta_source_health` tracks live reconnect state** (MAN-73, closing
  MAN-64's one-sided-gauge finding): every reconnectable source kind
  (Kiwi, HPSDR, Soapy, a plain audio device — everything but file replay)
  is wrapped at startup by `manta-cli::reconnect::ReconnectingSource`,
  which owns this gauge for that source's whole process lifetime. It
  reads `0` while the source is down and retrying with backoff (reusing
  `manta-server::backoff`'s 1s-60s policy) and `1` once samples are
  flowing again — including the very first connection, for a source kind
  (HPSDR) where opening a socket proves nothing about a device actually
  being present (see `IqSource::confirmed_live_handle`). File replay's
  health is set `1` once, immediately, and never read again: its errors
  and EOF are deterministic and must reach `listen()` unchanged, so it is
  never wrapped. A source that keeps failing to reopen now retries
  forever instead of ending the daemon — see
  `docs/DECISIONS/2026-10-05-man73-source-reconnect.md`. The
  `manta_input_*` series sum every connection a reconnectable source makes
  (MAN-228, `manta-cli::reconnect::InputHealthTotals`), so they keep
  counting across a reconnect and never reset mid-process.
  **MAN-64's terminal write** covers the one exit `ReconnectingSource`
  cannot report: when `manta_engine::listen` itself returns an error (file
  replay failed, a reopened source came back at a different sample rate
  or centre frequency, or the pipeline behind the source failed),
  `Command::Listen` writes a final `0` for the source before signalling the
  shutdown drain, through `Metrics::set_source_health_terminal` so no later
  regular write can flip it back. A clean end (EOF, Ctrl-C) leaves the
  gauge alone. **A scrape can see that `0` only while the metrics listener
  is still up**, and that window is not guaranteed: the listener runs
  outside `ClientTasks`, so it outlives the write only while a
  telnet/JSON/WS client is still draining under
  `shutdown_runtime_after_drain` (up to `SHUTDOWN_DRAIN_DEADLINE`); with
  no such client connected, the ordinary state for a scrape-only
  deployment, the runtime tears down microseconds later. See
  `docs/DECISIONS/2026-09-04-man64-metrics-request-rate-and-source-health.md`.
- Every dropped/evicted/suppressed item is counted. **No silent loss anywhere in
  the pipeline** — if coverage was bounded, the metrics say so.

## 9. Test strategy (`manta-testkit`)

The decisive advantage of building this in this ecosystem: **synthetic ground
truth with realistic HF impairment already exists.**

- **Synthetic CW generator**: text → keyed envelope (configurable WPM, weighting,
  rise-time/click shaping, human timing jitter model) → complex tone at arbitrary
  offset. Compose *many* generators into one wideband IQ scene ("50 signals,
  10–35 WPM, −5 to +30 dB SNR, 200 Hz–96 kHz spread").
- **Impairments from `coppa-channel`**: AWGN, frequency offset/drift, and the
  **Watterson HF model** (the standard ionospheric fading/multipath model) —
  reused, not rebuilt. manta accuracy is quoted *under Watterson CCIR-poor*,
  not just clean AWGN.
- **Golden IQ corpus**: recorded band segments (contest weekends = dense QRM;
  quiet weekdays = weak-signal) with RBN's own spots for the same time/frequency
  window as reference labels → recall/precision vs. the incumbent, the headline
  benchmark for M3.
- Unit level: proptest round-trips (text → CW → decode == text) across the
  WPM/SNR envelope; criterion benches gate the CPU budget (§4).
- End-to-end: daemon run from an IQ file must produce byte-identical spot logs
  across platforms (determinism requirement; no wall-clock in the decode path).

## 10. Concurrency model

- SDR/input thread → `rtrb` ring → **channelizer thread** (owns PFB, detector) →
  per-track sample queues → **decoder pool** (rayon-style fixed worker pool,
  tracks are work items; decoders are `Send` state machines, no shared state) →
  crossbeam channel → **tokio runtime** (validator, servers, metrics, control).
- Rationale: identical to coppa's proven audio/engine split — real-time DSP on
  dedicated threads with lock-free handoff; everything with a socket lives in
  async-land.
