# MAN-116: `manta bench sensitivity`, a standing recall/CER-vs-SNR curve

The 2026-09-05 broad review (D-18; lens 3 hit-list item #20) needed an
uncommitted scratch crate to measure how manta's recall and copy accuracy
fall off with SNR. Nobody could regenerate those numbers or check a
sensitivity claim against them later. `manta bench sensitivity` is the
standing tool: it synthesises recordings for every combination of channel
condition, sending speed and SNR, decodes each one in-process with the same
`manta_engine::decode_samples` pipeline `manta decode` uses, scores the
result against the ground truth, and prints a recall/CER-vs-SNR table with
SNR quoted in the 500 Hz reference bandwidth RBN and CW Skimmer use.

```
manta bench sensitivity [--conditions awgn,good,poor] [--wpm 15,25,35]
                        [--snr-db 0,3,6,9,12,15,20,25,30] [--duration-s 60]
                        [--trials 1] [--seed 1] [--engine …] [--config …]
                        [--jobs N] [--json | --markdown]
```

Code: `crates/manta-testkit/src/sensitivity.rs` (scenes, seeds, the
render-once-and-scale contract), `crates/manta-cli/src/bench.rs` (flags,
runner, scoring, the three renderers), wired from `crates/manta-cli/src/main.rs`.
Tests: `sensitivity::tests` (testkit), `bench::tests` (manta-cli unit), and
`crates/manta-cli/tests/bench_sensitivity.rs` (end to end through the binary).

## Decisions

- **D1: Command shape.** `manta bench sensitivity`, a nested subcommand like
  `manta config check`, named as the broad review proposed. It leaves room for
  a later `manta bench parity`. `manta bench` alone lists its subcommands.
- **D2: Scene.** Ten stations per recording, all at the point's condition,
  speed and SNR. Station *j* (0–9) sits at `−36000 + 8009.375·j` Hz from a
  14 MHz centre, at 96 kS/s. The extra 9.375 Hz per step is a tenth of the
  93.75 Hz channel, so the ten stations sample ten positions within their
  channels and average out channel-position effects (MAN-103's near-edge bug
  class). Spacing is ≥ 85 channels, far outside the ~2.5-channel interaction
  range; the station nearest DC is 3962.5 Hz away. Payload
  `CQ CQ DE <call> <call> K`, looped for the whole recording (the V8 payload,
  so the repetition gate sees two copies per message). Machine keying: no
  jitter, weight 3, gaps 3/7 units, 5 ms edges. Calls:
  `pileup_calls()[(trial·10 + j) mod 50]`. Ten stations per decode cost about
  one decode (1.1–1.5 s for ten against ~1.7 s for one in
  `snr_calibration.rs`), and the wideband path is what the product runs.
- **D3: SNR axis in 500 Hz, converted at the CLI boundary.** `--snr-db` and
  the `SNR` column are 500 Hz values. The runner converts with
  `snr_2500 = snr_500 − manta_server::rbn::RBN_REF_BW_CORRECTION_DB` (made
  `pub` for this, as MAN-102 D-102.4 widened a shared constant before). The
  testkit keeps its frozen 2500 Hz convention, mirroring broad-review D3's
  "convert only at the output boundary". JSON carries both values.
- **D4: Render once per series, scale per SNR (common random numbers).** A
  series is (trial, condition, speed). Its ten stations are rendered once at
  the 0 dB/2500 reference with no noise; the noise is rendered once per
  trial; each SNR point is `clean · 10^(snr_2500/20) + noise`. `render_scene`
  fades before it adds noise and applies `MASTER_SCALE` linearly, so this is
  exact to float precision: measured max |diff| / peak ≤ 2.24e-7 (AWGN) and
  ≤ 4.51e-7 (Watterson poor) against a direct render, with identical decoded
  text; `composed_recording_matches_a_direct_render` pins it at 1e-5. Fading
  *render*, not decode, dominates cost (60 s, 8 stations: AWGN render 0.72 s /
  decode 1.34 s; Watterson good 15.81 s / 1.13 s; Watterson poor 16.96 s /
  1.53 s), so this cuts the default grid from ≈1370 to ≈245 CPU-seconds.
  Points then differ only in SNR, which keeps curves smooth.
- **D5: Seeds.** SplitMix64 (`mix`: golden-ratio increment, then the
  finalizer); `derive_seed(seed, trial, stream) = mix(mix(mix(seed) ^ trial) ^ stream)`.
  Stream 0 is the trial's noise; stream *j*+1 is station *j*'s Watterson
  seed, the same number for good and poor. Seeds depend on (seed, trial,
  station) only, never on SNR, speed or condition.
  `seed_derivation_is_pinned` pins five values so an accidental change cannot
  silently move published numbers.
- **D6: Scoring.** As in the metric definitions below. A station's frequency
  is `14_000_000 + offset`. A track's frequency is its last
  `TrackMeta.freq_hz`, else its `TrackPromoted.freq_hz`; a track with neither
  is ignored. The CER copy joins the text of every track within 150 Hz, in
  ascending `track_id`, with single spaces: fading fragments one station into
  several tracks, and `golden_v8_v8w.rs`'s nearest-single-track rule would
  score a fragmented station as lost even when its copy is fine. Per decode,
  bogus calls are a sorted set. A decode `Err` whose message starts with
  `no signal found` is an empty decode (zero recall, CER 1.0 for every
  station, no bogus, as `char_gap_sweep.rs` treats it); any other `Err`
  aborts the run naming the point.
  `no_signal_is_an_empty_decode_but_digital_silence_is_an_error` pins the
  prefix contract against the real engine.
- **D7: Parallelism and determinism.** Series run sequentially. Inside a
  series, `std::thread::scope` renders stations in chunks of `--jobs`, and
  the per-station buffers are added into the series accumulator in station
  order, so float summation order never depends on `--jobs`. SNR points are
  decoded in chunks of `--jobs` and collected in point order. `--jobs`
  defaults to `std::thread::available_parallelism()`, which honours Linux
  cgroup quotas. Memory: ≈100 MB base plus ≈150 MB per job per minute of
  recording (46 MB per 60 s buffer, ≈95 MB per in-flight decode, ≈92 MB
  transient per in-flight Watterson station render).
- **D8: Streams.** stdout carries only the result (table, `--markdown` or
  `--json`); stderr carries the start line, per-point progress and the wall
  time. No clock, host, CPU count or absolute path reaches stdout; the config
  path is printed as typed. `--jobs` is left out of `Regenerate:` and the
  JSON `command` because it cannot change the result.
- **D9: Canonical lists.** Each list flag takes one comma-separated value
  through a newtype parser with `allow_hyphen_values`: clap 4.6.7's
  `value_delimiter` with `allow_negative_numbers` rejects `--snr-db -10,20`
  as `unexpected argument '-1'`. Duplicates and out-of-range values are clap
  usage errors (exit 2) naming the flag. Results are ordered awgn → good →
  poor, speed ascending, SNR ascending, whatever order was typed.
- **D10: Config.** `--config` uses `config::load(path, Env::Ignore)`.
  `[decode]` and `[detector]` apply, with `--engine` overriding only the
  engine. Other tables get one `note: bench sensitivity ignores [...]` line.
  `[input].freq_correction_ppm` is ignored because the recordings are
  synthetic; the `[spot]` lists are ignored because an allowlist would bypass
  validation and inflate recall.
- **D11: Placement.** Scene construction (pure, decode-free) lives in
  `manta_testkit::sensitivity`; the testkit cannot depend on `manta-engine`
  (the engine dev-depends on the testkit). The runner, scoring, renderers and
  flags live in `manta-cli/src/bench.rs`, the one crate that already links
  both.
- **D12: Instrument, baseline recorded.** The command exits 0 whatever the
  numbers are; it measures, it does not gate. This record carries the
  default-grid `--markdown` output at landing, with the manta version and the
  `Regenerate:` line but no commit hash: the commit that adds this record
  identifies the code.
- **D13: Validation ranges.** Speed: whole numbers 5–60 WPM. SNR: −30 to
  60 dB, at most one decimal. Duration: whole seconds 10–300 (below ~10 s no
  station completes a message and spots; above 300 s memory per job passes
  ~750 MB). Trials 1–20. Jobs ≥ 1. Every list non-empty, no duplicates.

The guarantee is "same build, same flags, same bytes" for any `--jobs`. No
cross-platform byte-identity is claimed: recordings use platform `f64`
`sin`/`cos`/`ln`, not verified across macOS and Linux.

## Metric definitions

Every number comes from in-process synthetic recordings
(`manta_testkit::sensitivity`), decoded by `manta_engine::decode_samples`
with `PipelineConfig::default()` (or `--engine`/`--config`) and validated by
the production `manta_spot::Validator` inside `decode_samples`. Nothing is
read from disk.

| Column | Definition |
|---|---|
| condition | `awgn`: no fading. `good`/`poor`: coppa `watterson_preset` `Good` (0.5 ms, 0.1 Hz) / `Poor` (2 ms, 1 Hz), an independent realization per station. |
| speed | Keyed WPM, PARIS timing, weight 3, 3/7-unit gaps, 5 ms edges, no jitter. |
| SNR | Key-down carrier power (before fading; under fading, the ensemble-mean power) over noise power in 500 Hz. Equals `snr_2500_db + 10·log10(5)`. |
| recall | Stations with at least one validated spot whose call equals the station's call and whose frequency is within 150 Hz of the station's, over stations. A real call spotted more than 150 Hz away is bogus, not recall. |
| bogus | Distinct callsigns among one decode's validated spots that are not a correct spot for any station. With `--trials`, each trial's distinct bogus calls are pooled (a call bogus in two trials counts twice; JSON `bogus_calls` lists them sorted). |
| CER | Per station: `manta_testkit::cer::cer(keyed_text, copy)`, capped at 1.0, where the copy is defined in D6. The column is the mean over stations (and trials). The ~2 s detector warm-up the keyed text includes puts a floor of about 0.03 under it at 60 s (0.07 at 30 s), as in every existing CER gate. |
| CER<0.10 | Stations whose capped CER is under 0.10: the same shape as the open V8w bar "≥ 90 % of signals at CER < 10 %". |
| spot SNR | For each spotted station, the mean of `Spot.snr_db + 6.9897` over its correct spots; then the mean over spotted stations. Whole dB in the table (`-` when no station was spotted), full precision in JSON. It shows manta's SNR reporting bias on synthetic signals. |

`--json` prints one object (`format_version` 1, a manta-local report format,
not the dispensa spot schema) with the run's parameters, the `command` that
regenerates it, and per point both `snr_db` (500 Hz) and `snr_2500_db`, the
aggregate columns, and `per_station` detail (`call`, `offset_hz`, `spotted`,
`spots`, `first_spot_s`, `spot_snr_db`, `cer`).

## Scope relative to MAN-102 FU-5

MAN-102's follow-up FU-5 assumed MAN-116 would calibrate the residual SNR
bias against RBN archive spots. This ticket is the synthetic half: the
`spot SNR` column measures manta's reporting bias on signals of known SNR.
The real-data half needs paired recorded IQ and RBN archive data, the MAN-20
parity benchmark's data dependency (ROADMAP M3), and moves there (FU-2
below).

## Baseline at landing

`manta 0.1.0`, run 2026-10-10 on the implementation branch as
`./target/release/manta bench sensitivity --markdown` (default grid: 81
points). Wall time was 66 s at 6 jobs in a container with a 6-CPU cgroup
quota; planning measured 136 s at 2 jobs. The rows matched the planning
prototype cell for cell. Verbatim stdout:

**manta 0.1.0 sensitivity sweep**: legacy engine, default decoder settings · 10 stations per point · 60 s recordings · 1 trial · seed 1

SNR: transmitted carrier against noise in 500 Hz, as RBN and CW Skimmer quote it.

| Condition | Speed | SNR | Recall | Bogus | CER | CER<0.10 | Spot SNR |
|---|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 15 WPM | 0 dB | 0/10 | 0 | 0.93 | 0/10 | - |
| AWGN | 15 WPM | 3 dB | 3/10 | 1 | 0.54 | 1/10 | 7 dB |
| AWGN | 15 WPM | 6 dB | 7/10 | 0 | 0.13 | 9/10 | 9 dB |
| AWGN | 15 WPM | 9 dB | 10/10 | 0 | 0.03 | 10/10 | 11 dB |
| AWGN | 15 WPM | 12 dB | 10/10 | 0 | 0.03 | 10/10 | 14 dB |
| AWGN | 15 WPM | 15 dB | 10/10 | 0 | 0.03 | 10/10 | 17 dB |
| AWGN | 15 WPM | 20 dB | 10/10 | 0 | 0.03 | 10/10 | 22 dB |
| AWGN | 15 WPM | 25 dB | 10/10 | 0 | 0.03 | 10/10 | 27 dB |
| AWGN | 15 WPM | 30 dB | 10/10 | 0 | 0.03 | 10/10 | 31 dB |
| AWGN | 25 WPM | 0 dB | 0/10 | 0 | 1.00 | 0/10 | - |
| AWGN | 25 WPM | 3 dB | 3/10 | 1 | 0.39 | 1/10 | 6 dB |
| AWGN | 25 WPM | 6 dB | 10/10 | 0 | 0.11 | 9/10 | 8 dB |
| AWGN | 25 WPM | 9 dB | 9/10 | 0 | 0.13 | 9/10 | 11 dB |
| AWGN | 25 WPM | 12 dB | 10/10 | 0 | 0.03 | 10/10 | 14 dB |
| AWGN | 25 WPM | 15 dB | 10/10 | 0 | 0.03 | 10/10 | 16 dB |
| AWGN | 25 WPM | 20 dB | 10/10 | 0 | 0.03 | 10/10 | 21 dB |
| AWGN | 25 WPM | 25 dB | 10/10 | 0 | 0.03 | 10/10 | 26 dB |
| AWGN | 25 WPM | 30 dB | 10/10 | 0 | 0.03 | 10/10 | 31 dB |
| AWGN | 35 WPM | 0 dB | 0/10 | 0 | 1.00 | 0/10 | - |
| AWGN | 35 WPM | 3 dB | 1/10 | 0 | 0.71 | 0/10 | 6 dB |
| AWGN | 35 WPM | 6 dB | 8/10 | 0 | 0.20 | 7/10 | 8 dB |
| AWGN | 35 WPM | 9 dB | 10/10 | 0 | 0.03 | 10/10 | 10 dB |
| AWGN | 35 WPM | 12 dB | 10/10 | 0 | 0.03 | 10/10 | 13 dB |
| AWGN | 35 WPM | 15 dB | 10/10 | 0 | 0.03 | 10/10 | 16 dB |
| AWGN | 35 WPM | 20 dB | 10/10 | 0 | 0.03 | 10/10 | 21 dB |
| AWGN | 35 WPM | 25 dB | 10/10 | 0 | 0.03 | 10/10 | 26 dB |
| AWGN | 35 WPM | 30 dB | 9/10 | 0 | 0.04 | 10/10 | 31 dB |
| Watterson good | 15 WPM | 0 dB | 0/10 | 0 | 0.84 | 0/10 | - |
| Watterson good | 15 WPM | 3 dB | 0/10 | 0 | 0.89 | 0/10 | - |
| Watterson good | 15 WPM | 6 dB | 1/10 | 0 | 0.87 | 0/10 | 10 dB |
| Watterson good | 15 WPM | 9 dB | 4/10 | 0 | 0.59 | 0/10 | 12 dB |
| Watterson good | 15 WPM | 12 dB | 6/10 | 0 | 0.20 | 3/10 | 16 dB |
| Watterson good | 15 WPM | 15 dB | 9/10 | 0 | 0.21 | 5/10 | 17 dB |
| Watterson good | 15 WPM | 20 dB | 9/10 | 0 | 0.06 | 9/10 | 22 dB |
| Watterson good | 15 WPM | 25 dB | 9/10 | 0 | 0.05 | 10/10 | 27 dB |
| Watterson good | 15 WPM | 30 dB | 9/10 | 0 | 0.06 | 9/10 | 32 dB |
| Watterson good | 25 WPM | 0 dB | 0/10 | 0 | 0.76 | 0/10 | - |
| Watterson good | 25 WPM | 3 dB | 2/10 | 0 | 0.70 | 0/10 | 8 dB |
| Watterson good | 25 WPM | 6 dB | 6/10 | 0 | 0.56 | 0/10 | 8 dB |
| Watterson good | 25 WPM | 9 dB | 6/10 | 0 | 0.39 | 0/10 | 12 dB |
| Watterson good | 25 WPM | 12 dB | 10/10 | 0 | 0.22 | 3/10 | 14 dB |
| Watterson good | 25 WPM | 15 dB | 10/10 | 0 | 0.11 | 4/10 | 16 dB |
| Watterson good | 25 WPM | 20 dB | 10/10 | 0 | 0.06 | 10/10 | 21 dB |
| Watterson good | 25 WPM | 25 dB | 10/10 | 0 | 0.05 | 10/10 | 26 dB |
| Watterson good | 25 WPM | 30 dB | 10/10 | 0 | 0.06 | 9/10 | 30 dB |
| Watterson good | 35 WPM | 0 dB | 0/10 | 0 | 0.82 | 0/10 | - |
| Watterson good | 35 WPM | 3 dB | 3/10 | 0 | 0.64 | 0/10 | 8 dB |
| Watterson good | 35 WPM | 6 dB | 7/10 | 0 | 0.53 | 0/10 | 9 dB |
| Watterson good | 35 WPM | 9 dB | 10/10 | 1 | 0.32 | 0/10 | 12 dB |
| Watterson good | 35 WPM | 12 dB | 10/10 | 1 | 0.16 | 2/10 | 14 dB |
| Watterson good | 35 WPM | 15 dB | 10/10 | 0 | 0.10 | 6/10 | 16 dB |
| Watterson good | 35 WPM | 20 dB | 10/10 | 0 | 0.06 | 10/10 | 21 dB |
| Watterson good | 35 WPM | 25 dB | 10/10 | 0 | 0.06 | 9/10 | 25 dB |
| Watterson good | 35 WPM | 30 dB | 9/10 | 0 | 0.08 | 6/10 | 30 dB |
| Watterson poor | 15 WPM | 0 dB | 0/10 | 0 | 0.98 | 0/10 | - |
| Watterson poor | 15 WPM | 3 dB | 0/10 | 0 | 1.00 | 0/10 | - |
| Watterson poor | 15 WPM | 6 dB | 0/10 | 1 | 0.97 | 0/10 | - |
| Watterson poor | 15 WPM | 9 dB | 0/10 | 0 | 0.82 | 0/10 | - |
| Watterson poor | 15 WPM | 12 dB | 0/10 | 0 | 0.64 | 0/10 | - |
| Watterson poor | 15 WPM | 15 dB | 1/10 | 1 | 0.44 | 0/10 | 17 dB |
| Watterson poor | 15 WPM | 20 dB | 6/10 | 0 | 0.22 | 0/10 | 23 dB |
| Watterson poor | 15 WPM | 25 dB | 8/10 | 0 | 0.14 | 3/10 | 29 dB |
| Watterson poor | 15 WPM | 30 dB | 7/10 | 0 | 0.10 | 5/10 | 34 dB |
| Watterson poor | 25 WPM | 0 dB | 0/10 | 0 | 0.95 | 0/10 | - |
| Watterson poor | 25 WPM | 3 dB | 0/10 | 0 | 0.99 | 0/10 | - |
| Watterson poor | 25 WPM | 6 dB | 0/10 | 0 | 0.88 | 0/10 | - |
| Watterson poor | 25 WPM | 9 dB | 0/10 | 0 | 0.64 | 0/10 | - |
| Watterson poor | 25 WPM | 12 dB | 0/10 | 0 | 0.48 | 0/10 | - |
| Watterson poor | 25 WPM | 15 dB | 1/10 | 0 | 0.36 | 0/10 | 12 dB |
| Watterson poor | 25 WPM | 20 dB | 4/10 | 0 | 0.25 | 0/10 | 22 dB |
| Watterson poor | 25 WPM | 25 dB | 6/10 | 0 | 0.14 | 1/10 | 28 dB |
| Watterson poor | 25 WPM | 30 dB | 7/10 | 0 | 0.29 | 2/10 | 32 dB |
| Watterson poor | 35 WPM | 0 dB | 0/10 | 0 | 0.93 | 0/10 | - |
| Watterson poor | 35 WPM | 3 dB | 0/10 | 0 | 0.86 | 0/10 | - |
| Watterson poor | 35 WPM | 6 dB | 0/10 | 0 | 0.66 | 0/10 | - |
| Watterson poor | 35 WPM | 9 dB | 0/10 | 0 | 0.54 | 0/10 | - |
| Watterson poor | 35 WPM | 12 dB | 4/10 | 0 | 0.41 | 0/10 | 12 dB |
| Watterson poor | 35 WPM | 15 dB | 6/10 | 0 | 0.32 | 0/10 | 17 dB |
| Watterson poor | 35 WPM | 20 dB | 7/10 | 0 | 0.21 | 0/10 | 22 dB |
| Watterson poor | 35 WPM | 25 dB | 8/10 | 0 | 0.17 | 1/10 | 28 dB |
| Watterson poor | 35 WPM | 30 dB | 3/10 | 0 | 0.24 | 1/10 | 33 dB |

- **Recall**: stations spotted with the right call, within 150 Hz of where they sent.
- **Bogus**: calls spotted that no station sent on that frequency.
- **CER**: character error rate of each station's copy, capped at 1, averaged.
- **CER<0.10**: stations copied with a character error rate under 0.10.
- **Spot SNR**: average SNR manta reported on its correct spots, in 500 Hz; - when none.

Each station sends "CQ CQ DE &lt;call&gt; &lt;call&gt; K" for the whole recording. AWGN is a steady signal in white noise; Watterson good and poor add two-path HF fading (0.5 ms / 0.1 Hz and 2 ms / 1 Hz) to each station independently.

Regenerate: `manta bench sensitivity --markdown`

### Reading the baseline

- **AWGN** recall steps up between 3 and 6 dB: 3/10 → 7/10 at 15 WPM,
  3/10 → 10/10 at 25 WPM, 1/10 → 8/10 at 35 WPM. From 9 dB up it is 10/10
  everywhere except two 9/10 cells (25 WPM at 9 dB, 35 WPM at 30 dB). CER
  reaches its ≈0.03 warm-up floor by 9 dB (15 and 35 WPM) or 12 dB (25 WPM).
- **Watterson good** reaches 9–10/10 recall 0–6 dB later than AWGN (at 15,
  12 and 9 dB for 15, 25 and 35 WPM; 15 WPM never passes 9/10), and needs
  20 dB before 9–10/10 stations are copied at CER < 0.10.
- **Watterson poor never reaches 10/10 recall** in the default grid. Nothing
  is spotted below 12 dB; the best cell is 8/10 (15 and 35 WPM at 25 dB), and
  at most 5/10 stations are copied at CER < 0.10 (15 WPM at 30 dB). This is
  the fading-robustness gap MAN-107–113 target.
- **Recall falls again at the top of the range under fading.** Watterson poor
  at 35 WPM drops from 8/10 at 25 dB to 3/10 at 30 dB, and Watterson good at
  35 WPM loses clean copies (CER < 0.10 from 10/10 at 20 dB to 6/10 at
  30 dB). FU-1.
- **Spot SNR reads high.** Under AWGN from 9 dB up, manta reports 1–2 dB above
  the true SNR (e.g. 15 WPM: 11, 14, 17, 22, 27, 31 dB for 9…30 dB),
  consistent with MAN-102's measured +1…+2 dB residual. Near the threshold
  (3–6 dB) it reads 2–4 dB high because only stations caught on favourable
  noise get spotted. Under Watterson poor from 20 dB up it reads 2–4 dB
  high (e.g. 15 WPM: 23, 29, 34 dB at 20, 25, 30 dB).
- **Bogus calls** are rare on this scene: one call at 6 of the 81 points, none
  above 15 dB.

## Follow-up register

Lifted from the MAN-116 plan for the merge-time filer.

- **FU-1: High-SNR fading recall collapse.** In the default grid, Watterson
  poor at 35 WPM drops from 8/10 at 25 dB to 3/10 at 30 dB (500 Hz). A
  planning prototype counted 41–98 decoded tracks for 10 stations at 30 dB
  under fading (against 10–12 at 20 dB). Likely the same family as
  `snr_calibration.rs`'s "no spots at 30 dB/2500" cliff. Decoder/detector
  work; `manta bench sensitivity --conditions good,poor --snr-db 20,25,30,35,40`
  reproduces it.
- **FU-2: MAN-102 FU-5 real-data half.** Characterize the residual SNR bias
  against RBN archive spots on recorded IQ. Belongs with the MAN-20 parity
  benchmark (needs its data); the bench's `spot SNR` column covers the
  synthetic half.
- **FU-3: Optionally move `golden_v8_v8w.rs`** onto shared in-process scoring
  (it shells out to the binary and uses nearest-single-track CER). Not needed
  for MAN-116.
- **FU-4: Extensions.** `moderate` Watterson, `--jitter`, adjacent-station
  spacing sweeps, a first-spot-latency column (JSON `first_spot_s` already
  carries it), `--keep-recordings DIR`.
- **FU-5: A regression gate on the curve** (e.g. a CI or nightly check that
  AWGN recall stays 10/10 from 9 dB up), once the curve is stable through
  MAN-107–113.
- **FU-6: Release runbook step**, if the owner wants every release to publish
  a curve (`docs/RUNBOOKS/release.md`).
