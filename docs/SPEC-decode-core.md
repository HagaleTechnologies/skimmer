# SPEC — Decode Core (channelizer → detector → demod → Morse decode)

Status: draft v1. Companion to `ARCHITECTURE.md` §4–§5. This document is the
implementation-level specification: an implementer should be able to code
`manta-dsp` and `manta-decode` from this file without making design
decisions. All constants are normative defaults; every one is exposed in the
TOML config under the names given in [§9](#9-configuration-keys).

Deviations from ARCHITECTURE.md are marked **[DEVIATION]** and summarized in
[§10](#10-deviations-from-architecturemd).

Notation: `fs` = input complex sample rate (S/s), `N` = FFT/channel count,
`Δ` = channel spacing (Hz), `m` = hop index, `k` = channel index `0..N-1`,
`hop` = samples advanced per PFB output frame.

---

## 1. Channelizer (4×-oversampled polyphase filterbank)

### 1.1 Dimensions

`N = fs / 93.75`, which is a power of two for all supported rates:

| Input rate `fs` | `N` | Spacing `Δ = fs/N` | `hop = N/4` | Output rate `fo = fs/hop` | Hop period |
|---|---|---|---|---|---|
| 96 000  | 1024 | 93.75 Hz | 256  | 375 Hz | 2.667 ms |
| 192 000 | 2048 | 93.75 Hz | 512  | 375 Hz | 2.667 ms |
| 384 000 | 4096 | 93.75 Hz | 1024 | 375 Hz | 2.667 ms |
| 768 000 | 8192 | 93.75 Hz | 2048 | 375 Hz | 2.667 ms |

Non-power-of-two input rates (e.g. KiwiSDR 12 kHz IQ) are rational-resampled
in `manta-input` to the nearest table rate before the channelizer;
the channelizer itself only ever sees a table rate. Audio-passband mode
(48 kHz real) is Hilbert-transformed then treated as `fs = 48 000`, `N = 512`
(same 93.75 Hz spacing; the table generalizes as `N = fs/93.75`).

All downstream timing constants are defined in milliseconds and converted to
hops at `fo = 375 Hz` exactly once, at startup, rounding half-up to the
nearest integer hop (the single normative rounding rule for all ms→hop
conversions in this spec). `fo` is invariant across input
rates by construction; nothing below depends on `fs`.

### 1.2 Prototype filter

**[DEVIATION]** `coppa-dsp::filter` provides only `RrcFilter` (root-raised
cosine); there is no general Kaiser/windowed-sinc designer to reuse. The
prototype designer is therefore new code in `manta-dsp::proto` (~40 lines:
Kaiser window + windowed sinc). `coppa-dsp::fft::FftProcessor` is reused as
specified.

Prototype lowpass, length `L·N` with **L = 8 taps per branch**:

- Windowed-sinc: `h[n] = sinc(2·f_c·(n − (LN−1)/2) / fs) · w_kaiser[n]`,
  `n = 0..LN-1`, normalized so `Σ h[n] = 1` (unity DC gain per channel).
- Cutoff `f_c = Δ/2 = 46.875 Hz` (−6 dB point at the channel edge; adjacent
  channels cross at −6 dB, giving the required ≥50 % spectral overlap so a
  signal straddling an edge loses ≤ 3 dB in the better channel after the §1.4
  interpolation).
- Kaiser `β = 7.857` (target stopband attenuation **A = 80 dB**;
  `β = 0.1102·(A − 8.7)`). With `LN ≥ 8192` taps at 96 kS/s the transition
  band is ≈ 60 Hz — stopband is reached by ~107 Hz offset, i.e. alias
  rejection ≥ 80 dB from 1.15 channels away.
- Coefficients are computed in `f64` and stored as `f32`, generated once at
  startup; the same function is unit-tested against fixed reference values
  (first/middle/last 4 taps at N=1024 pinned to 1e-7).

### 1.3 Structure and per-hop processing (WOLA form)

Maintain a sliding input window `x[0..LN)` (newest sample at the end). Every
`hop` new samples:

1. **Window & fold:** `u[n] = x[n] · h[LN−1−n]` for `n = 0..LN`, then fold to
   `N` points: `v[j] = Σ_{p=0..L-1} u[j + pN]`, `j = 0..N`.
2. **Phase correction for hop < N:** circularly rotate `v` left by
   `r = (m · hop) mod N` samples: `v'[j] = v[(j + r) mod N]`. (Equivalent to
   multiplying bin `k` by `e^{+j2πk·m·hop/N}`; the rotation keeps every
   channel's passband centered at DC in its own output stream so the envelope
   is phase-continuous across hops.)
3. **FFT:** `X[k, m] = FFT_N(v')[k]` via `FftProcessor::new(N)` (one instance,
   created once; `forward()` allocates — acceptable at 375 calls/s, but the
   engine may pre-allocate via `try_forward` into a scratch buffer if profiling
   demands; not required for M0/M1).
4. **Power:** `P[k, m] = |X[k, m]|²`. Report in dB: `PdB = 10·log10(P + ε)`,
   `ε = 1e-20`.

Channel `k` corresponds to RF frequency
`f(k) = f_center + ((k + N/2) mod N − N/2) · Δ` (standard FFT bin order,
negative frequencies in the upper half). All detector/decoder code works in
channel index; conversion to Hz happens only when a spot is emitted.

### 1.4 Fine frequency estimate (for ±10 Hz spot accuracy)

Per hop, for a track with peak channel `k₀`:

- Quadratic interpolation on **dB** powers of `(k₀−1, k₀, k₀+1)`:
  `δ_m = 0.5·(P₋ − P₊) / (P₋ − 2P₀ + P₊)` where `P• = PdB[k₀•, m]`.
  Clamp `δ_m` to `[−0.5, +0.5]`; if the denominator ≥ 0 (no local max — a
  peak requires `P₋ − 2P₀ + P₊ < 0`), set `δ_m = 0` and mark the hop unusable.
- Only **key-down** hops (§3.4) with `SNR ≥ 6 dB` contribute.
- Track centroid: power-weighted running mean over the track lifetime:
  `C = Σ (k₀ + δ_m)·P₀[m] / Σ P₀[m]` (accumulate in `f64`).
- Spot frequency: `f_spot = f(0) + C·Δ` rounded to **0.01 kHz** for the
  telnet output (MAN-88 — RBN's live feed carries 2 decimals of kHz; the
  previous one-decimal rounding discarded 10 Hz of the estimator's real
  precision), full precision (Hz) in the JSON stream.

With ≥ 100 key-down hops (any real CW transmission) the estimator's standard
error is ≪ 10 Hz; absolute accuracy is then bounded by the SDR's reference
oscillator, which is out of scope (config `input.freq_correction_ppm` exists).
This ≪ 10 Hz claim is empirically channel-table-size (N) dependent: it was
measured at N = 1024 (the 96 kHz table V1 uses); V31's 48 kHz/N = 512 table
measures ~17 Hz instead (see the V31 row below; tracked in issue #177).

### 1.5 Decimation (variable-width capture, issue #169)

An optional stage between the `IqSource` and the channelizer: a cascade
of `log2(factor)` Kaiser-windowed halfband FIR decimate-by-2 stages,
`factor` restricted to powers of two. Each stage's ideal cutoff sits at
`CUTOFF_FRACTION * fs_in` (0.235, deliberately below the theoretical
quarter-band point `fs_in/4` -- the output Nyquist after that stage's
decimate-by-2 -- reserving a transition-band margin so full stopband
attenuation is actually reached by the new Nyquist rather than only
somewhere past it), Kaiser-windowed at the same beta/stopband target as
the channelizer prototype (§1.2's `KAISER_BETA`, 80 dB). The decimated
rate must itself satisfy §1.1's `fs/93.75` power-of-two table constraint.
Module: `manta-dsp::decimate`.

A halfband filter has every other tap forced to zero by construction
(except the center tap), which in principle halves the multiply-accumulate
cost per output sample. The current implementation does not exploit this:
`HalfbandStage::process` iterates all taps unconditionally, including the
structurally-zero ones, so the 2x MAC saving is not yet realized. This is a
known, deliberate gap for now (tracked in issue #176), not an oversight,
and there is likewise no criterion bench yet measuring this stage's cost
against the repo's Pi4 CPU budget. Moving the cutoff off exactly `fs_in/4`
(above) also gives up this exact-zero-tap property, so issue #176's skip
opportunity no longer applies to this stage regardless.

**Known limitation** (issue #179): the `CUTOFF_FRACTION` margin means
channels near the decimated Nyquist edge see real, non-negligible
attenuation (roughly -6 dB to -22 dB in the last ~1.5 kHz below the edge,
for a 96k->48k stage) while still being exposed to the channelizer as
ordinary trackable channels -- a real CW signal landing there can lose
enough SNR to go undetected. The channelizer has no concept of decimation
and does not yet exclude or de-weight these transition-band channels.

---

## 2. Noise floor & signal-presence detection

### 2.1 Per-channel floor estimator

Order-statistic estimator over a sliding window, computed from a decimated
power stream to bound cost:

- Every 15th hop (25 Hz), push `PdB[k, m]` into a per-channel ring of
  **250 entries (10 s)**.
- Maintain a per-channel histogram of the ring contents: 0.5 dB bins spanning
  −140..0 dBFS (280 bins, `u8` counts; increment on push, decrement on evict —
  O(1) per update, no sorting).
- Floor `F_ch[k]` = the **25th percentile** of the histogram. Median is NOT
  used: CW key-down duty cycle reaches 50–60 % on a busy channel, which
  inflates the median by the full signal power; the lower quartile stays on
  the noise rail unless duty exceeds 75 %.

### 2.2 Neighborhood floor and effective floor

A channel occupied continuously for > 10 s still inflates its own quartile.
Guard with a spectral-neighborhood floor:

- Group channels into blocks of 32. `F_blk[b]` = median of the 32 `F_ch`
  values in block `b`, recomputed at 25 Hz.
- **Effective floor:** `F[k] = min(F_ch[k], F_blk[⌊k/32⌋] + 3 dB)`.

The +3 dB allowance tolerates genuine floor slope across a block (e.g. filter
edges); the `min` guarantees a parked carrier can never raise its own
detection threshold by more than 3 dB relative to its neighbors.

Startup: for the first 10 s the ring is partially filled; the quantile is
taken over whatever is present, and track creation is inhibited for the first
**2 s** (`detector.warmup_ms = 2000`) to avoid floor-transient garbage.

### 2.3 Gate

Per channel, smoothed power `S[k, m]`: EMA of `PdB[k, m]` with time constant
**τ = 40 ms** (`α = 1 − e^{−2.667/40} = 0.0645`).

- **Rise:** `S ≥ F + 6 dB` (`detector.on_snr_db = 6.0`) sustained for
  **19 consecutive hops (≈ 50 ms)** — rejects impulse noise and clicks.
- **Drop:** `S < F + 3 dB` (`detector.off_snr_db = 3.0`, i.e. 3 dB
  hysteresis) continuously for **hang = 5 000 ms** (1875 hops) — survives QSB
  troughs and inter-word gaps at slow speeds.

Reported track SNR (for spots) is converted from the 93.75 Hz channel to the
conventional 2500 Hz reference bandwidth:
`SNR_2500 = (S − F) − 10·log10(2500/93.75) = (S − F) − 14.3 dB`.

`S − F` is **peak-held over each `TrackMeta` reporting interval** (§5's 375
hops) before conversion: `S` is a τ = 40 ms EMA that decays toward the floor
on every key-up, so an instantaneous sample is a function of keying phase,
not of signal strength. The peak over the interval is the settled key-down
level (MAN-102; see
`docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md` for the
measured rejection of an instantaneous sample and of a key-down mean).

This 2500 Hz value is what `TrackMeta`, `Spot.snr_db`, and the JSON stream
carry. The telnet and RBN-uplink wire lines convert it to the 500 Hz
reference bandwidth RBN and CW Skimmer use (`+10·log10(2500/500) ≈ 6.99
dB`), per decision D3
(`docs/DECISIONS/2026-09-06-broad-review-decisions.md`) — a rendering step
at the output boundary only. The internal pipeline, §4.5's confidence `q`,
and §7's vector pass criteria are all unchanged and remain defined in
2500 Hz.

### 2.4 Track lifecycle state machine

States: `IDLE → CANDIDATE → ACTIVE → HANG → CLOSED`.

| Transition | Condition |
|---|---|
| IDLE → CANDIDATE | rise condition first met on channel `k`, and `k` is not owned by an existing track (§2.5) |
| CANDIDATE → ACTIVE | rise sustained 19 hops → lease decoder from pool |
| CANDIDATE → IDLE | rise condition lost before 19 hops |
| ACTIVE → HANG | drop condition met (below off threshold) |
| HANG → ACTIVE | `S ≥ F + on_snr_db` again (hang timer reset) |
| HANG → CLOSED | hang timer (5 000 ms) expires → decoder returned, final spots flushed |
| ACTIVE/HANG → CLOSED | **garbage collect:** no character emitted for 30 000 ms (`detector.gc_ms`) — carrier or non-CW signal; the channel is marked *suppressed* for 60 s (re-detection allowed but logged) |
| any → CLOSED | eviction: track cap reached and this is the lowest-SNR track (counted in metrics, per ARCHITECTURE §4) |

All timers are hop-counted (integers), never wall-clock.

### 2.5 Adjacent-channel ownership (one signal ⇒ one track)

- A track tracks a fractional center `c` (the §1.4 running centroid,
  initialized to its birth channel `k₀`). It **owns** channels
  `{round(c) − 1, round(c), round(c) + 1}`.
- A CANDIDATE in an owned channel is absorbed: no new track; ownership stays
  with the incumbent.
- Each hop, the track's demod input is taken from the **max-power channel
  among its owned set** (per ARCHITECTURE §4 "max-power selection"); the
  owned set follows `round(c)` as the centroid drifts, which is how drifting
  signals (§7 test 9) are followed without retuning.
- If two channels `k` and `k+1` meet the rise condition on the *same hop* and
  neither is owned, one CANDIDATE is created at the higher-power channel.
- Two tracks whose centers converge within 1.0 channel (interference or
  drift-collision) are merged: the lower-SNR track is CLOSED with reason
  `merged` (counted); its decoder state is discarded (text already emitted
  stands).

---

## 3. Per-track demodulation

Input: `a[m] = sqrt(max-power-owned-channel P)` — the linear envelope at
375 Hz. All constants below in ms are converted to hops (2.667 ms/hop).

### 3.1 Normalization

**[DEVIATION — narrowed]** `coppa-dsp::agc::AdaptiveAgc` is block-based
(`new(target_level, block_size)`, `process(&[f32])`) and designed for audio
block flows. At 375 Hz a meaningful block is 16 samples ≈ 43 ms of latency
and its adaptation interacts with the keying envelope itself. Since the §3.2
threshold is self-normalizing (it estimates both rails), AGC adds no decision
value. **Normalization is a single fixed scale per track:** divide `a[m]` by
`A_ref` = the 90th percentile of `a` over the first 500 ms after ACTIVE
(re-estimated once if `E_hi` later drifts above `3·A_ref` or below
`A_ref/3`). `AdaptiveAgc` is not used in the decode path.

### 3.2 Dual-EMA adaptive keying threshold

State: `E_hi` (key-down level), `E_lo` (key-up level). **[DEVIATION]**
(MAN-103, `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`): the
keying decision is not a single threshold but an additive band about the
linear-amplitude midpoint of the two rails, `mid = (E_hi + E_lo)/2`,
half-width `half = hyst_frac · (E_hi − E_lo)` (§3.3). The prior geometric
mean `T = sqrt(E_hi · E_lo)` (ARCHITECTURE §5's original text) sits closer
to `E_lo` than to `E_hi` as apparent keying depth grows, so a rising edge
only had to climb a small fraction of the amplitude range while a falling
edge had to decay nearly all the way back down — inflating every measured
mark, worse at higher SNR and near a channel edge (where the recovered
envelope's own rise/fall transient is slower). A band symmetric in *linear
amplitude* about the midpoint measures a symmetric transition's true
50%-crossing duration exactly, for any transition width or keying depth,
while the rails sit at the true mark and space levels (up to one hop of
quantization of the continuous crossing point). The rail-update split
below (MAN-213) lets transition samples pull `E_hi` slightly low, so slow
transitions read a little long. MAN-213 decision B3
(`docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md`)
measured the steady-state mark bias on a synthetic raised-cosine envelope:
none for ramps up to 6 hops (16 ms), 0 to +2 hops for 8–10-hop ramps and
+2 to +3 hops for a 12-hop ramp (a triangular pulse with no flat top),
against at most +1 hop with the pre-MAN-213 rails.
`keying_edge_placement_is_unbiased_across_depth_and_ramp` gates ±1 hop at
ramps of 2 and 6 hops and `0..=3` hops at 12.

Initialization, from the first 375 hops (1 s) after ACTIVE:
`E_hi = Q90(a)`, `E_lo = max(Q10(a), 1e-6)`. If `E_hi / E_lo < 2` (< 6 dB
apparent keying depth) the track stays in a *pre-decode* state and
re-attempts initialization every 1 s; no elements are emitted (prevents
decoding carriers/noise).

Per-hop update — **[DEVIATION]** (MAN-213,
`docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md`, superseding
MAN-103's D5 band-gated rail update). The key-decision band (§3.3) does
**not** gate rail updates: under that rule a mark that fades to about half
its pre-fade amplitude sits inside the band, so neither rail updates, `E_hi`
freezes at its pre-fade level, the key never goes down and the faded marks
are lost. Every sample instead updates exactly one rail, split at the
geometric mean `T_cls = sqrt(E_hi · E_lo)` — the pre-MAN-103 classification.
On top of that split, a **fade re-anchor**: a sustained run of at least
`debounce_hops` (§3.3, 12 ms) consecutive samples between the old key-down
threshold `1.25 · T_cls` and the key-down bound `mid + half` is a faded mark,
and sets `E_hi` to their mean, so the band follows the fade down. This
restores the pre-MAN-103 `1.25·T` fade margin without moving the key
decision itself (§3.3 is unchanged). All of `mid`, `half` and `T_cls` are
computed from the rails before this hop's update:

```
mid = (E_hi + E_lo) / 2 ; half = hyst_frac * (E_hi - E_lo)
T_cls = sqrt(E_hi * max(E_lo, 1e-6))
if a[m] > T_cls: E_hi ← E_hi + α_hi · (a[m] − E_hi)
else:            E_lo ← E_lo + α_lo · (a[m] − E_lo)
# fade re-anchor: n consecutive hops with 1.25·T_cls < a ≤ mid + half,
# n ≥ max(debounce_hops, 1)  →  E_hi ← mean(a over those n hops)
```

The re-anchor fires on every hop while the run stays at or above
`debounce_hops` (the run is not reset when it fires); a `debounce_ms` that
rounds to 0 hops still needs one in-zone sample. The one-shot `A_ref`
re-estimation (§3.1) rescales the run's accumulated sum by the same factor as
the rails.

Time constants: `τ_lo = 500 ms` fixed
(`α_lo = 1 − e^{−2.667/500} = 0.00532`).
`τ_hi` is WPM-adaptive once speed is tracked (§4.1):
`τ_hi = clamp(5 · dit_ms, 100, 400) ms`, initial 200 ms. Rationale: `E_hi`
must ride QSB (fast) but average over several elements (≥ 5 dits) so a single
stretched dah doesn't drag it.

Floors: `E_hi ≥ 2·E_lo` is enforced after every update (if violated, set
`E_hi = 2·E_lo`); prevents rail collapse during long silences. It is also the
lowest value the fade re-anchor can drive `E_hi` to.

### 3.3 Key decision with hysteresis and debounce

- **[DEVIATION]** (MAN-103): key-down when `a[m] > mid + half`; key-up when
  `a[m] < mid - half` (recomputed from the current rails, after §3.2's
  update); between the two bounds the previous state holds. This band is the
  **key decision only** — it does not gate §3.2's rail updates (MAN-213).
  `hyst_frac = 0.15` (band = 35%..65% of the keying depth `E_hi − E_lo`).
  Replaces the prior `1.25·T` / `0.80·T` multiplicative band: that band was
  symmetric in the *log* domain, not the linear one, so it reintroduced the
  very rise/fall asymmetry an unbiased threshold placement removes. For a
  transition that is symmetric in time (true here: the PFB prototype filter
  is linear-phase, and the testkit's raised-cosine keying edges are
  symmetric), the rise-crossing delay equals the fall-crossing delay *iff*
  the up/down thresholds are placed symmetrically about the amplitude
  midpoint — and then the measured mark equals the true 50%-point mark
  exactly, for any transition width and any keying depth, as long as the
  rails sit at the true levels, up to one hop of quantization (§3.2 states
  the slow-ramp residual its rail-update split leaves). `hyst_frac` is
  purely a noise-immunity knob: the underlying timing is provably
  independent of the band width (confirmed by a real-pipeline sweep over
  0.05–0.45 that left on-centre WPM readings unchanged).
- **Debounce:** a run (mark or space) shorter than **12 ms (≈ 4.5 hops)** is
  merged into its neighbors (the two adjacent runs and the short run become
  one run of the neighbors' polarity). 12 ms ≈ half a dit at 50 WPM — nothing
  legitimate is that short.

### 3.4 Element stream

Output of this stage: alternating `Mark(duration_hops)` /
`Space(duration_hops)` events, timestamped by the sample counter of their
leading edge. A space is not emitted until the next mark begins (open-ended
trailing space is flushed as a word boundary by the 7-dit timeout rule,
§4.2).

---

## 4. Element classification & Morse decoding

### 4.1 Online 2-means speed tracking (marks)

State: `μ_dit`, `μ_dah` in ms. Boundary between clusters:
`B = sqrt(μ_dit · μ_dah)` (geometric mean ≈ `1.73·μ_dit` at nominal 3:1).

**Initialization** — after the first **5 marks**: sort durations;
find the largest ratio gap between consecutive sorted values. If
`max/min ≥ 2.0`, split at the largest gap: `μ_dit` = mean below,
`μ_dah` = mean above. Otherwise (all one cluster — e.g. `EEE` or `TTT`):
`μ_dit = mean`, `μ_dah = 3·μ_dit` provisionally, flagged *unconfirmed* until
a mark lands ≥ `2·μ_dit` (then it re-anchors: `μ_dah = that duration`).

Special case: an all-dah opening (plausible for `CQ` at the margin) corrects
itself via the constraint clamp below the first time a real dit arrives —
the dit falls far below `μ_dit`, gets assigned to the dit cluster, drags it
down, and the clamp re-anchors `μ_dah`.

**Update** — per mark of duration `d`: assign to the nearer cluster in log
space (i.e. `dit` iff `d < B`), then EMA the assigned centroid:
`μ ← μ + 0.15·(d − μ)`.

**Constraints** — after every update enforce `2.2 ≤ μ_dah/μ_dit ≤ 4.5`
(weighted keying and Farnsworth stay inside this window); on violation,
re-anchor `μ_dah = 3·μ_dit`. Clamp `μ_dit` to `[20 ms, 150 ms]`
(60 WPM .. 8 WPM); the reported PARIS WPM is `1200 / dit_estimate`
(§4.1a's symmetric correction of `μ_dit_ms`), EMA-smoothed with
`α = 0.1`.

**Drift/regime change** — if 12 consecutive marks assign to a single cluster
*and* their coefficient of variation < 0.35 *and* their mean is off that
centroid by > 40 %, the operator has changed speed (QRQ/QRS): reinitialize
from the last 5 marks. (Plain EMA tracking already follows ≤ ~20 % gradual
drift; this rule catches step changes.) A regime-change reinit also resets
the §4.1a gap centroid below (a stale correction from the old speed regime
must not carry into the new one).

#### 4.1a WPM reporting: symmetric mark/gap dit-period estimate

**[DEVIATION]** (MAN-103, `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`):
the PARIS WPM report does not use `μ_dit` directly. A keying-edge crossing
only moves the mark/space *boundary* — it neither creates nor destroys time
— so `μ_dit + μ_egap = 2 · true_dit` regardless of where that boundary sits.
This holds even for a *correctly-placed* §3.2/§3.3 threshold, because the
testkit's (and any real transmitter's) raised-cosine keying edge is
"contained inside the element": the true 50%-point mark is one full rise
time shorter than its nominal keyed length, and the following gap is
exactly that much longer. A threshold-placement fix alone (§3.2/§3.3) removes
the *SNR/offset-dependent* error term but leaves this *constant*
transmitter-shaping term untouched — left alone, it reads high by roughly
one rise time's worth of WPM at every speed.

Track a second EMA centroid, `μ_egap` (SPEC §9 `cluster_alpha`), over gaps
classified `InterElement` (§4.2), and **only** those. Gaps the §4.2 flush
safety net resolves directly are never element gaps (the safety net fires at
`7·μ_dit`); folding them in here would pin `dit_estimate` at its
`+DIT_BIAS_CAP_FRAC` cap and read WPM ~26 % low. §4.2's flush note feeds
those gaps to the *Farnsworth long-gap* statistics, which is a different
estimator. The dit period used for reporting is:

```
delta = clamp(0.5 * (mu_dit - mu_egap), -DIT_BIAS_CAP_FRAC * mu_dit, +DIT_BIAS_CAP_FRAC * mu_dit)
dit_estimate = clamp(mu_dit - delta, DIT_CLAMP_MS)
wpm_raw = 1200 / dit_estimate
```

with `DIT_BIAS_CAP_FRAC = 0.35`, a two-sided cap (bounding a runaway if
mark/gap pairing ever breaks down, e.g. element gaps swallowed by the 12 ms
debounce at extreme WPM). `μ_dit` itself (and its boundary `B`) stays
uncorrected: §4.2's `u = gap_ms/μ_dit`, §4.3's beam likelihoods, and §3.2's
`τ_hi` all want a centroid consistent with the marks actually being
classified — only the WPM *report* wants the absolute physical estimate.
Before any element gap has been observed, `dit_estimate = μ_dit` (no
correction).

### 4.2 Gap classification (spaces)

Nominal thresholds in dits (`u = gap_ms / μ_dit`):

- `u < 2.0` → **inter-element** (within a character)
- `2.0 ≤ u < 5.0` → **inter-character**
- `u ≥ 5.0` → **inter-word**

The implementation matches this nominal `2.0` boundary (`CHAR_GAP_DITS` in
`crates/manta-decode/src/timing.rs`). It was lowered to `1.6` for a period
(`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`) to compensate for
§3.2's old geometric-mean threshold systematically inflating measured
`μ_dit`; MAN-103 fixed that threshold at its source (§3.2/§3.3), so the
compensation this deviation existed for is gone. The 2026-07-18 sweep
methodology was re-run against the corrected timing before restoring `2.0`
(500 cases x 2 independent seeds, at both the envelope and the full IQ layer;
table in `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`, harness
in `crates/manta-engine/tests/char_gap_sweep.rs`): `1.6` no longer buys
anything at either layer, and `2.0` is the middle of a flat region running
from `1.6` to at least `2.5`.

**Farnsworth decoupling** (ARCHITECTURE §5.3): run the same 2-means machinery
on the "long gaps" — those with `u ≥` the inter-character boundary above,
i.e. exactly the gaps this section does *not* call inter-element. The floor
and the character boundary are one boundary seen from two sides and must
always carry the same value; a floor below the boundary admits ordinary
element gaps (which measure `u ≈ 1.5`–`1.6` near 40 WPM) into the long-gap
clusters and drags the word threshold down onto real character gaps
(MAN-103). Yields `μ_cgap` (character gap)
and `μ_wgap` (word gap) when bimodal. Once ≥ 8 long gaps have been observed
and `μ_wgap / μ_cgap ≥ 1.8`, the word threshold becomes the geometric mean
`sqrt(μ_cgap · μ_wgap)` instead of the fixed `5.0` dits; the character
threshold stays at `2.0` dits (element/character confusion is speed-locked;
character/word spacing is what Farnsworth stretches).

A trailing space reaching `7·μ_dit` without a new mark forces character +
word flush immediately (don't wait for the next mark to close a word —
spots must not lag the transmission). **[DEVIATION]** (MAN-103 D8): this
flush resolves its gap *outside* the classification path above, so it must
separately fold that gap into the Farnsworth long-gap statistics
(`long_seen`, `μ_cgap`/`μ_wgap`) or the Farnsworth bootstrap can never
complete once `μ_dit` runs at its corrected (non-inflated) value — with the
old, inflated `μ_dit`, `flush_gap_dits · μ_dit` sat comfortably above real
Farnsworth character gaps and this never mattered; with the corrected value
it can drop below them, so the safety net would otherwise intercept nearly
every character gap before `classify` ever saw it.
The folded value is the flush threshold, not the gap's closed length (the
space is still open), so a pause between calls cannot inflate `μ_wgap`.
Under heavy Farnsworth spacing the character gap itself outruns the flush,
and that censoring hides the spacing: every character is flushed as its own
word. So the closed lengths of the last five gaps flushed after decoded
one-character words re-initialize the long-gap pair. A word of two or more
characters, or a flushed garble, clears that window. The rebuild happens
only on a clean split: the largest-ratio split leaves at least two gaps in
each cluster, max/min is `≥ 2`, and the clusters' nearest members differ by
`≥ 1.8` (MAN-213; MAN-103 D8 resolution). A rebuilt pair is discarded, and
the long-gap statistics restart, when a classified gap of at least `2.0` but
under `5.0` dits arrives before five confirming word gaps. An ordinary
character gap contradicts the rebuild's premise that every character gap
outran the flush. With `flush_gap_dits` below `5.0`, the upper bound is
the larger of `flush_gap_dits` and the rebuilt low cluster over `1.8`: a
character gap above both outran the flush and belongs to the low cluster, so
it is consistent with the rebuild. A confirming word gap, classified or
flushed, ends a decoded word of two or more characters. Gaps after decoded
one-character words built the pair, so they never confirm it. Under a false
rebuild, one-character words at ordinary spacing decode as one merged word,
so a pause after such a run does confirm it. After five confirmations the
rebuild stands, so a dit lost to a fade inside a character cannot discard
it (MAN-264).

### 4.3 Per-element likelihoods

Marks are modeled log-normally about their centroid with fixed log-domain
σ = 0.25:

```
ll(d | dit) = −(ln d − ln μ_dit)² / (2·0.25²)
ll(d | dah) = −(ln d − ln μ_dah)² / (2·0.25²)
```

(σ = 0.25 ⇒ ±28 % duration at 1σ — measured human keying jitter is 10–20 %,
QSB-induced edge erosion adds the rest. **This is the riskiest constant in
the spec**; it is config `decode.timing_sigma` and must be validated against
the golden corpus at M3.)

Spaces inside a character contribute no score (already classified by §4.2);
the character boundary decision itself is hard, not beamed — beaming gap
types explodes state for negligible gain at CW speeds.

### 4.4 Beam search over the Morse tree (width 4)

The code tree: root, dit = left child, dah = right child; nodes carry an
optional glyph. Standard table A–Z, 0–9, `. , ? / = + - ( ) @ : ; ' " _ $ !`
plus prosign terminal nodes: `AR` (`.-.-.`), `SK` (`...-.-`), `BT` (`-...-`,
same node as `=`, emitted as `=`), `KN` (`-.--.`, same node as `(`), `AS`
(`.-...`), `VE/SN` (`...-.`). Prosigns emit as text tokens `<AR>` `<SK>`
`<AS>` `<SN>` in the JSON stream and are dropped from the telnet-facing text.

**[DEVIATION — narrowed]** The beam is **character-local**: it resets to the
tree root at every character boundary, and inter-character continuity is
greedy (winning character is committed). ARCHITECTURE §5.4 could be read as a
transmission-length beam; word-level ambiguity is the validator's job
(cty.dat / SCP context in `manta-spot`), and a character-local beam makes
determinism and confidence bookkeeping trivial. Cross-character correction is
explicitly out of scope for the classical decoder.

Per character:

1. Start with one hypothesis: `(node = root, score = 0)`.
2. For each mark `d` in the character: every hypothesis branches to its
   dit-child (score `+ ll(d|dit)`) and dah-child (score `+ ll(d|dah)`).
   A branch into a nonexistent child (sequence longer than any code, > 7
   elements) is dropped; if *all* branches drop, the character aborts as
   garble (emits nothing, counts as a decode error for confidence).
3. Prune to the best **4** hypotheses by score after each mark.
4. At the character boundary: surviving hypotheses whose node has no glyph
   are dropped; if all four are glyphless, the character emits `?` with
   confidence 0. The winner is the highest-score glyph-bearing hypothesis.

**Error prosign:** a mark run of ≥ 6 dits-classified marks with no dah
(operator sending `........`) emits control token `<ERR>`; the validator
discards the current word buffer back to the previous word boundary.

### 4.5 Per-character confidence

Softmax over the final hypothesis scores `s₁ ≥ s₂ ≥ …` (the ≤ 4 survivors,
plus dropped-at-boundary hypotheses excluded):

```
c_char = exp(s₁) / Σᵢ exp(sᵢ)          (∈ (0, 1], =1 if single survivor)
c_char ← c_char · q,  q = clamp(SNR_2500 / 20 dB, 0.3, 1.0)
```

`q` folds channel quality in so that a clean-timed character in the mud never
reaches full confidence. Emitted per character in the decoder output stream.

`q`'s `SNR_2500` is still the demod's own §3.2 keying-rail estimate
(`Demod::snr_2500_db`), not `TrackMeta.snr_2500_db`'s §2.3 floor-based value
below — deliberately (MAN-102 / decision D3): the two were the same value by
coincidence before MAN-102, and are now intentionally decoupled, not merged.

### 4.6 Per-callsign confidence (consumed by `manta-spot`)

**[DEVIATION]** `r` is no longer strictly "on the track" -- MAN-166,
2026-09-09 (`docs/DECISIONS/2026-09-09-man166-confirm-hops-and-track-cap.md`,
`crates/manta-spot/src/gate.rs`). A real signal's `track_id` changes every
time its track closes and reopens (e.g. a 5s silence timer), which a
literal per-track `r` would reset on every churn regardless of whether the
same callsign was still genuinely repeating -- confirmed as a real bug
against a genuine 40m contest recording. `r` is counted per
(frequency-bucket, callsign) instead, which survives that churn; a decode
from a *different* track_id within a reasoned minimum gap of the most
recent one is still rejected as a likely concurrent duplicate (two tracks
decoding the same real transmission), while the same track_id always
counts (one decode stream can't decode the same instant twice) --
`RepetitionGate::record`'s own doc has the full mechanism.

For a candidate callsign of `n` characters with confidences `c₁..c_n`,
decoded `r` distinct times within the 90 s window:

```
c_call = (Π cᵢ)^(1/n) · (1 − 0.5^r)
```

Geometric mean (one garbled character tanks it, correctly) times a
repetition factor: `r=1 → 0.5`, `r=2 → 0.75`, `r=3 → 0.875`. The validator's
own adjustments (cty/SCP hits, per ARCHITECTURE §6) multiply on top of this
and are specified in `manta-spot`, not here. The ≥ 2-repetition gate for
first spot is unchanged for non-beacon, non-allowlisted spot types; a
message already type-tagged `BEACON` by the context parse (ARCHITECTURE §6
step 1), or a callsign the operator has explicitly allowlisted (ARCHITECTURE
§6's Watch List), is exempt from this gate — never needs a second, distinct
decode (MAN-28). `r` still feeds `c_call` above unchanged, so a spot of
either kind still carries the `r=1` confidence penalty at the repetition
count it actually resolved with. An allowlisted callsign also bypasses
ARCHITECTURE §6 steps 1 (context parse -- tagged `SpotType::Unknown` when no
CQ/DE/UP/beacon pattern matched) and 2 (grammar/cty) entirely, and may spot
the instant its pattern completes, as before.

**Beacon emission timing (amended 2026-09-09, see
`docs/DECISIONS/2026-09-09-beacon-emission-deferred-to-track-close.md`):** a
non-allowlisted `BEACON`-tagged candidate no longer spots "on the first
decode" in the sense of immediately as it's parsed. It is captured (grammar/
cty/blocklist/notch checked immediately, as always) but held until the
track's true close (`TrackClosed`), and only then evaluated against the
track's true final reported speed (an implausibly fast final speed --
`manta-spot`'s `MAX_PLAUSIBLE_WPM`, a validator-local heuristic, not a SPEC
value -- permanently discards it; nothing here is retried). This was found
necessary in practice: a live WPM reading taken at the moment of first
decode is not yet the track's true, settled value, and gating emission on
it (in either direction) reopened exactly the noise-artifact false-positive
problem the heuristic exists to close. The repetition-gate exemption itself
is unchanged -- a non-allowlisted beacon still never needs a second, distinct
decode -- only the MOMENT of emission moved from "first decode" to "track
close." An allowlisted callsign is unaffected by this and still spots
immediately.

**"Distinct" (MAN-100).** Two decodes of the same callsign text on a track
count as separate repetitions toward `r` only when at least
`MIN_MESSAGE_WORD_GAP = 3` decoded words on that track separate them, **or**
at least `MIN_MESSAGE_TIME_GAP_SECONDS = 60` seconds of `sample_ts` separate
them (MAN-100 remediation C2). SPEC's own default payload template repeats
the callsign back-to-back within one transmission (`CQ CQ DE <CALL> <CALL>
K`, §7's payload note) — without the word-gap half of this rule, that
single, possibly fading-corrupted message alone could satisfy the ≥
2-repetition gate. 3 words sits strictly between the one-word gap inside a
single message and the minimum five-word gap between two separate ones
(`<CALL> K CQ CQ DE <CALL>`). That word-gap reasoning assumes SPEC's own
payload template, though, and does not hold for a real, shorter ID (e.g.
"DE `<CALL>`", 2 words) — the time-gap half exists for exactly that case: 60
s comfortably covers a full "CQ CQ DE `<CALL>` `<CALL>` K" transmission even
at 8 WPM (this section's slowest supported speed, ~40 s for that template)
with margin, while staying well under the 90 s ledger/gate window itself.
The beacon/allowlist exemptions above are unaffected — they never consult
`r`'s distinctness rule at all. A short "DE `<CALL>`" ID repeated only
twice at ordinary (sub-60 s) cadence remains unspotted under this rule — an
accepted, bounded recall cost (MAN-100 remediation C2, quantified; V43),
not tightened further: any time-gap threshold low enough to rescue it would
also treat a single corrupted message's own doubled utterance as two
distinct messages, reopening the hole this rule exists to close.

**Cross-candidate variant arbitration (MAN-100), ARCHITECTURE §6 step 4b.**
Before a candidate spots, it is checked against every other decoded,
spottable-shaped word observed on the same track within the same 90 s
window. It is withheld if a confusable, better-supported rival exists —
"confusable" meaning a shared contiguous substring relationship, or a
shared prefix of at least 3 characters with edit distance ≤ 2 — where
"better-supported" means strictly more message-distinct repetitions (ties
broken by summed per-occurrence confidence), or the candidate being a
strict prefix of a rival that has been observed at all (≥ 1 message-distinct
repetition — shape alone decides a prefix-containment pair once the rival
exists, however little support it has). An earlier attempt (MAN-100
remediation C5) also required the rival to independently clear the same
≥ 2-repetition floor a spottable candidate must, on the reasoning that a
single stray, garbled decode that happens to be a textual prefix-extension
of a well-supported candidate should not be enough on its own to veto it;
reverted in remediation round 3 because it excluded the ticket's own
measured case (a 3-rep truncation losing to a genuine, longer call that
had only a single observation on the track) — the two shapes are
numerically indistinguishable from the ledger alone, and the measured,
real case takes priority over the unmeasured, synthetic one that motivated
C5. Symmetrically, a rival that is itself a strict prefix of the
candidate never wins this comparison on repetition count alone (MAN-100
remediation C1) — shape decides a prefix-containment pair in both
directions, not just when the shorter form is being arbitrated. This
mechanism is purely subtractive: it can only withhold a spot the rest of
this section would otherwise emit, never produce one, and it never fires
against an operator-allowlisted callsign, one present in the bundled SCP
list, or a candidate already type-tagged `BEACON` (MAN-100 remediation C3 —
the same once-per-cycle reasoning as this section's own beacon
repetition-gate exemption above: a beacon's structurally low rep count
would otherwise let a confusable, fading-corrupted rival permanently
outrank it). The per-track ledger this arbitration reads evicts an entry
once its newest observation ages out of the 90 s window (MAN-100
remediation C6), so a long-lived track's key space stays bounded by what's
currently live rather than growing with track history. See
`manta-spot::variant`/`manta-spot::support` for the exact relation and
comparison, and the MAN-100 decision record for the measured rationale
behind the prefix-only asymmetry.

---

## 5. Decoder output

Per track, an ordered event stream:

```
CharDecoded    { track_id, sample_ts: u64, char: char | Token, confidence: f32 }
WordBoundary   { track_id, sample_ts: u64 }
SpeedUpdate    { track_id, wpm: f32 }          (emitted on ≥ 1 WPM change)
TrackMeta      { track_id, sample_ts: u64, snr_2500_db: f32, freq_centroid: f64 }  (1 Hz cadence)
TrackPromoted  { track_id, sample_ts: u64, freq_hz: f64 }  (detector-internal;
                 added post-freeze, 2026-09-09 — the exact hop a track is
                 promoted from CANDIDATE to ACTIVE, independent of whether the
                 decoder subsequently produces anything. See
                 docs/DECISIONS/2026-09-09-doctor-track-promoted-event.md.)
TrackClosed    { track_id }  (added post-freeze, MAN-19 — a track has closed
                 and will never emit another event under this track_id; only
                 emitted for a track that produced at least one other event
                 first.)
```

`TrackMeta.snr_2500_db` is §2.3's floor-based `S − F` estimate (peak-held
over the reporting interval, per §2.3), supplied by the detector layer that
owns the gate/floor state -- not the §3.2 keying-rail ratio §4.5's `q` uses
(MAN-102 / decision D3).

`TrackMeta.sample_ts` is the hop that produced it, so §6 rule 6's
`(sample_ts, track_id)` resequencing places it in its true chronological
position -- not a synthetic tie value -- among the same batch's
`CharDecoded`/`WordBoundary` events (MAN-102 review round 2, finding 1:
tying it to a synthetic `0` let a just-reported SNR retroactively attach to
characters decoded earlier in the same batch, with the effect's magnitude
depending on the caller's chunk size).

`sample_ts` is the input-stream sample counter (u64, monotonic from stream
start). Wall-clock time exists only at the spot-emission boundary
(`manta-server`), derived as `stream_start_time + sample_ts / fs` where
`stream_start_time` comes from config/file sidecar — never from `Instant::now()`
inside the decode path. A live source's disconnect/reconnect (MAN-73) reports
the missed span, in samples, via `IqSource::take_discontinuity()`; `listen()`
advances the sample clock by that count and starts a fresh track segment, so
`sample_ts` stays monotonic and wall-clock-true across the gap with no audio
spliced in. File replay never reports a discontinuity.

---

## 6. Determinism requirements

A daemon run from an IQ file MUST produce byte-identical decoder output (and
therefore spot logs) across runs and platforms (ARCHITECTURE §9). Normative
rules:

1. **No RNG** anywhere in `manta-dsp` / `manta-decode`. (The testkit's
   jitter models use seeded `rand_chacha`; seeds are part of the test vector.)
2. **No wall clock** in the decode path (§5). All timers are hop/sample
   counters.
3. **Fixed iteration order:** tracks are processed in ascending birth order
   (track_id, a monotonic u32) each hop; channels in ascending index. Any map
   keyed by track/channel in an output-affecting path is a `BTreeMap` or
   sorted `Vec`, never a `HashMap` iterated.
4. **Float discipline:** all per-sample state is `f32` with a fixed operation
   order (no fast-math, no FMA-dependent reductions: the fold in §1.3 and the
   centroid in §1.4 accumulate in `f64` sequentially). `rustfft` is
   deterministic per-platform for power-of-two sizes; cross-platform FFT
   bit-equality is NOT assumed — the byte-identical requirement applies to
   like-for-like builds, and cross-platform equality is asserted at the
   *decoded-text* level (test vectors, §7), not the sample level.
5. **Beam tie-break:** equal scores order by (element-sequence lexical order,
   dit < dah). Softmax computed with the max-subtraction trick, fixed order.
6. **Pool scheduling must not affect output:** decoder workers may run in any
   order, but each track's decoder is a pure function of its own input queue;
   emitted events are sequenced by `(sample_ts, track_id)` before the
   validator sees them.

CI enforces: same binary + same IQ file, 3 runs → identical SHA-256 of the
JSON spot log; and the §7 vectors on all platforms.

---

## 7. Golden test vectors (M0/M1 acceptance)

All generated by `manta-testkit`: text → keyed envelope (raised-cosine
edges, 5 ms rise/fall, per-element timing jitter σ = 8 % where stated) →
complex tone at the stated offset → impairments via `coppa-channel`
(`awgn(seed)`; Watterson via the streaming `WattersonChannel` API per
SPEC-watterson §6 — not the deprecated one-shot helper). SNR is quoted
**in 2500 Hz**.
Every vector: `fs = 96 000`, 120 s scene unless stated, fixed seeds recorded
in the fixture manifest. Text payload (unless stated):
`CQ CQ DE <CALL> <CALL> K` repeated for the scene duration.

| # | Name | Signal(s) | Impairment | Pass criteria |
|---|---|---|---|---|
| V1 | clean-20 | 20 WPM, +20 dB, offset +12.34 kHz, W1AW | AWGN only, no jitter | char accuracy = 100 %; 1 track; freq error ≤ 10 Hz |
| V2 | fast-35 | 35 WPM, +15 dB, JA1ABC | AWGN, jitter 8 % | char ≥ 99 %; WPM reported 35 ± 2 |
| V3 | slow-weak | 12 WPM, +6 dB, VK9DX | AWGN, jitter 8 % | char ≥ 95 %; callsign validated (≥ 2 reps) |
| V4 | fade-good | 25 WPM, +10 dB, DL1ABC | Watterson CCIR-good | char ≥ 95 % |
| V5 | fade-poor | 22 WPM, +3 dB, ZL2XYZ | Watterson CCIR-poor | char ≥ 80 %; callsign validated within 90 s |
| V6 | qsb-sine | 20 WPM, envelope ×(0.55 + 0.45·sin 2π·0.2t) (≈ +20→0 dB), K5ZZZ | AWGN | char ≥ 90 %; track survives (no CLOSED before end) |
| V7 | adjacent | 24 WPM @ +10.000 kHz and 28 WPM @ +10.150 kHz, both +15 dB, calls N1AA / N2BB | AWGN | exactly 2 tracks; both char ≥ 95 %; both freqs ± 15 Hz |
| V8 | pileup-50 | 50 signals, 10–35 WPM, −2..+25 dB, uniform over ±45 kHz, unique calls from fixture list | AWGN, jitter 8 % | ≥ 45/50 callsigns validated in 120 s; 0 bogus (non-fixture) callsigns spotted |
| V8w | pileup-50-fading | same scene as V8 | Watterson CCIR-poor, jitter 8 % | ≥ 90 % of signals with mean SNR ≥ +6 dB decoded with CER < 10 %; 0 bogus callsigns; 0 cross-channel ghost decodes |
| V9 | drift | 18 WPM, +12 dB, drift +50 Hz/min, EA8AAA | AWGN | 1 track (no split); char ≥ 90 %; final freq tracks within 15 Hz |
| V10 | farnsworth | 15 WPM chars / 25 WPM char-speed (Farnsworth), +15 dB, G4XXX | AWGN | char ≥ 95 %; word boundaries 100 % correct |
| V31 | decimated-clean-20 | Same scene as V1 (20 WPM, +20 dB, offset +12.34 kHz, W1AW), synthesized at 192 kHz then decimated to 48 kHz via `manta_dsp::decimate::Decimator` | AWGN only, no jitter | char ≥ 98%; 1 track; freq error ≤ 25 Hz |

V31's freq-error bound (25 Hz) differs from V1's (10 Hz) because the fine-
frequency estimator's error is channel-table-size dependent, not a
`Decimator` regression: measured ~17.4 Hz at the decimated path's N = 512
table size vs. V1's N = 1024 (96 kHz) table. Confirmed by two no-decimator
control renders of the same scene -- native 48 kHz and native 192 kHz both
independently measure a similar ~15-17 Hz error with zero decimator
involvement (see `crates/manta-cli/tests/golden_decimated_capture.rs`).

M0 = V1 passing end-to-end from a WAV file. M1 = V1–V6. V7–V10 and V8w gate M2
(multi-track engine). The RBN-parity corpus benchmark remains the M3 gate
(ARCHITECTURE §9) and is not redefined here. V31 gates variable-width capture
(issue #169); unlike V1–V10 it is a standalone test in
`crates/manta-cli/tests/golden_decimated_capture.rs`, not part of the
`manta-testkit::vectors` V1–V10 fixture table.

### 7.1 `manta-spot` validator vectors (M3 sub-project 1)

Unlike V1–V10 (testkit-synthesized IQ), these operate at the
`DecoderEvent`-stream level -- hand-built event sequences feeding
`Validator::ingest` directly, no IQ synthesis involved. V11-V15, V18-V45b
are implemented in `crates/manta-spot/tests/golden_v11_v15.rs`; V16-V17
(operator suppression, MAN-31 -- orthogonal to this pipeline, see
ARCHITECTURE §6) in `crates/manta-spot/tests/golden_v16_v17.rs`.

| # | Name | Scenario | Pass criteria |
|---|---|---|---|
| V11 | context-parse | Each of `CQ <call>`, `CQ TEST <call>`, `CQ <contest> <call>` (e.g. `CQ WPX`; filler set per `manta-spot::context`), `TEST <call>`, `DE <call>`, `<call> UP`, `V V V <call>`, `<call> T` | Correct `SpotType` assigned per pattern family |
| V12 | bogus-prefix | Structurally-valid callsign with a prefix absent from cty.dat | 0 spots, even though grammar passes |
| V13 | scp-boost | Same callsign/confidences with vs. without SCP membership | `c_call` strictly higher when a member; absence never rejects |
| V14 | repetition-gate | 1 decode vs. 2 decodes of the same callsign within 90 s, non-beacon spot type | 1 rep never spots; 2 reps does |
| V15 | dedupe | Repeat spot inside the 10 min window, then an SNR jump >= 6 dB | Suppressed inside the window; allowed after the SNR jump |
| V16 | bad-call blocklist | Callsign present vs. absent from the operator's bad-call list | Present → 0 spots; absent → spots normally |
| V17 | notched frequency | Track frequency inside vs. outside a notched range | Inside → 0 spots; outside → spots normally |
| V18 | beacon-repetition-exemption | 1 decode of a `V V V <call>` beacon pattern, track closed at a plausible speed | `BEACON`-tagged spot emits once the track closes -- repetition gate not applied regardless (MAN-28); emission TIMING moved to track-close 2026-09-09, see §4's amendment note -- no spot before `TrackClosed` |
| V19 | allowlist-bypass | A single decode of a callsign with an unallocated cty prefix, explicitly allowlisted | Spots despite failing grammar/cty and despite only 1 decode (MAN-28 Watch List) |
| V20 | allowlist-no-context | An allowlisted callsign decoded with no CQ/DE/UP/beacon framing at all | Spots, tagged `SpotType::Unknown` (MAN-28 Watch List, the primary NCDXF-beacon case) |
| V21 | allowlist-independent-of-context | A stale, already-attempted context match (e.g. `CQ K5ARH`, decoded once, never spotted) sits in the window when a different, freshly-allowlisted word arrives | The allowlisted word still spots -- context-match and allowlist candidates are evaluated independently, not one-or-the-other by priority (MAN-28 Watch List) |
| V22 | exempt-spot-waits-for-metadata | A first-decode-exempt callsign (BEACON or allowlist) decoded before the track's first `TrackMeta` event | No spot until real telemetry arrives; the pending candidate spots once metadata does arrive, on the next word boundary |
| V23 | allowlist-reclassification | An allowlisted call spots immediately with no context (type `Unknown`), then a trailing word completes a real context pattern (e.g. `<call> UP` -> `De`) | A second, corrected spot is emitted with the new type -- an already-attempted word is not permanently locked to its first type |
| V24 | reclassification-reps-reuse | "DE K5ARH" decodes once (reps=1, held back by the repetition gate), then a trailing `CQ` token reclassifies the same word from `De` to `Cq` | Still 0 spots -- the reclassification reuses the word's existing rep count rather than recording a second decode |
| V25 | metadata-retries-pending-candidate | A `TrackMeta` event arrives for a track with a pending exempt candidate (held back by V22) and no further word ever completes | The candidate is retried and spots as part of the `TrackMeta` ingest itself, not left waiting on a `WordBoundary` that may never come |
| V26 | reclassification-never-downgrades | "DE K5ARH" spots as `De`; 15 more words push "DE" out of the 16-word window while "K5ARH" remains | No spot reverts to `Unknown` -- reclassification only ever promotes a word's type, never downgrades one that already earned a contextual type |
| V27 | reclassification-never-downgrades-between-types | "CQ DE K5ARH" spots as `Cq`; 15 more words push both "CQ" and "DE" out of the window while "K5ARH" remains | No spot reclassifies to `De` -- the same aging-out bug shape as V26, for a pair of two contextual types instead of type-vs-`Unknown` |
| V28 | reclassification-still-accepted | "DE K5ARH" spots as `De`; a `CQ` token then arrives as a genuinely new trailing word (not via aging) | A second spot promotes it to `Cq` -- V26/V27's fix rejects aging-driven changes specifically, not reclassification in general |
| V29 | provenance-bound-to-occurrence | "CQ DE K5ARH K CQ DE K5ARH" repeats DE-K5ARH across two genuinely separate messages (MAN-100 Scenario 2 requires the gap); the newest K5ARH spots as `Cq` after 2 reps, then "CQ" and the first "DE" age out while the second "DE K5ARH" remains | No spot reclassifies to `De` -- provenance is bound to the exact word occurrence `evaluate_candidate` selects, not whichever occurrence the regex matched first |
| V30 | power-step-beacon-exemption | 1 decode of a `<call> T` power-step beacon pattern (MAN-37), track closed at a plausible speed | `BEACON`-tagged spot emits once the track closes, gate not applied regardless -- same exemption V18 proves for `V V V <call>`, extended to the power-step pattern; emission timing per V18's amendment note |
| V38 | variant-arbitration | A track decodes both a callsign and a confusable, less-supported variant of it (truncation or shared-prefix near-miss) inside one 90 s window -- variants V38b (per-track scoping) and V38c (a well-supported real call is not suppressed by a 1-rep head-merge artifact) | Only the better-supported candidate spots; arbitration is per track, and never fires against a form that could not itself be spotted |
| V39 | same-message-repetition | One `CQ CQ DE <CALL> <CALL> K` transmission, then a second, genuinely later one | The first message's doubled call alone never satisfies the ≥ 2-rep gate; the second message completes it |
| V40 | truncation-arrives-first | A strict-prefix truncation clears the repetition gate on a track before the genuine, longer call has any support at all, which then appears | The genuine call still spots once observed -- the prefix-containment asymmetry fires regardless of arrival order (MAN-100 remediation C1) |
| V41 | short-id-wide-time-gap | A 2-word ID ("DE `<CALL>`") repeated 80 s apart -- below `MIN_MESSAGE_WORD_GAP` but past `MIN_MESSAGE_TIME_GAP_SECONDS` | Still clears the repetition gate as two distinct messages (MAN-100 remediation C2) |
| V42 | beacon-exempt-from-arbitration | A confusable rival of a `BEACON`-tagged candidate reaches more reps than the genuine, once-per-cycle beacon | The genuine beacon still spots -- `BEACON` candidates are exempt from step 4b arbitration (MAN-100 remediation C3) |
| V43 | short-id-ordinary-cadence-unspotted | A 2-word ID ("DE `<CALL>`") repeated only twice, 20 s apart -- below both `MIN_MESSAGE_WORD_GAP` and `MIN_MESSAGE_TIME_GAP_SECONDS` | Not spotted -- an accepted, bounded recall cost (MAN-100 remediation C2, quantified), not tightened further |
| V44 | 1-rep-rival-still-wins-by-shape | The literal, measured V8w track-90 shape: a 3-rep truncation ("W6JQ") vs. its genuine, longer form ("W6JQA") observed only once on the track | The truncation is withheld -- shape decides a prefix-containment pair once the rival has been observed at all, regardless of how few reps it has (MAN-100 remediation round 3; a rival-side rep floor tried in remediation C5 excluded this exact case and was reverted) |
| V45 | non-callsign-convention | Each listed CW convention (`5NN`, `3NN`, `599`, `TEST`, …) sent as a CQ/DE/beacon candidate, some of which match an allocated cty.dat prefix (`5N` Nigeria) | 0 spots — rejected by grammar regardless of cty.dat (MAN-105) |
| V45b | convention-never-vetoes | A track that repeats `5NN` exchanges, then a real non-SCP call ending in `5NN` (`HA5NN`) | The real call spots — the convention never enters the MAN-100 support ledger (MAN-105) |

---

## 8. Module map (where each section lands)

| Spec section | Crate::module |
|---|---|
| §1 channelizer, prototype | `manta-dsp::pfb`, `manta-dsp::proto` |
| §1.4 interpolation | `manta-dsp::centroid` |
| §2 floor + gate + state machine | `manta-dsp::floor`, `manta-engine::track` |
| §3 demod | `manta-decode::envelope` |
| §4.1–4.2 timing | `manta-decode::timing` |
| §4.3–4.5 beam decode | `manta-decode::beam`, `manta-decode::tree` |
| §5 events | `manta-decode::events` |
| §7 vectors | `manta-testkit::vectors` |

## 9. Configuration keys

Every configurable constant above, with its real code default. The block
below is valid TOML that `manta` loads as-is (`manta decode --config` on
it decodes byte-identically to no config at all; `docs_consistency.rs`
checks both that and that each value equals the code default). Keys the
code still hard-codes are listed commented out, marked `not configurable
yet` -- the loader rejects them with that message rather than ignoring
them.

```toml
[detector]
# Bounds: 0 <= off_snr_db <= on_snr_db <= 100; every *_ms in [0, 3600000],
# converted with ms_to_hops (§1.1); confirm_ms/hang_ms/gc_ms must round to
# >= 1 hop; track_cap >= 1.
# Rise threshold, dB SNR. 12.0, not SPEC v1's 6.0: raised by
# docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md item 2 (§10).
on_snr_db = 12.0
off_snr_db = 3.0
confirm_ms = 50                     # 19 hops
hang_ms = 5000                      # 1875 hops
gc_ms = 30000                       # 11250 hops
warmup_ms = 2000                    # 750 hops
# Max concurrent tracks (ARCHITECTURE §4); 1200 per
# docs/DECISIONS/2026-09-09-man166-confirm-hops-and-track-cap.md.
track_cap = 1200
# MAN-171: a channel whose track closed Silent may not spawn a new
# CANDIDATE for this long.
silent_respawn_cooldown_ms = 30000  # 11250 hops
# floor_quantile = 0.25       # not configurable yet: compile-time constant in manta-dsp::floor
# floor_window_ms = 10000     # not configurable yet: compile-time constant in manta-dsp::floor
# block_channels = 32         # not configurable yet: compile-time constant in manta-dsp::floor
# block_allowance_db = 3.0    # not configurable yet: compile-time constant in manta-dsp::floor

[decode]
# SPEC-decode-core-v2.md §7 lists the v2 evidence/noise/HSMM keys.
engine = "legacy"           # "legacy" | "edge-legacy" | "hsmm"; --engine overrides
timing_sigma = 0.25
beam_width = 4
debounce_ms = 12
# Key-decision band half-width as a fraction of keying depth (§3.3,
# MAN-103); replaces v1's hyst_up/hyst_down. Must be > 0.0 and < 0.5.
hyst_frac = 0.15
tau_lo_ms = 500
tau_hi_bounds_ms = [100, 400]
flush_gap_dits = 7.0
# mu_ratio_bounds = [2.2, 4.5]  # not configurable yet: compile-time constant in manta-decode::timing
# char_gap_dits = 2.0           # not configurable yet: compile-time constant in manta-decode::timing (every engine, MAN-103)
# word_gap_dits = 5.0           # not configurable yet: compile-time constant in manta-decode::timing
# cluster_alpha = 0.15          # not configurable yet: compile-time constant in manta-decode::timing

[input]
# The source. Omit `type` (and every source key) to use the default audio
# device, or when command-line flags pick the source. Each type takes only
# its own source keys (required ones marked *):
#   audio: device
#   file:  path*, iq
#   kiwi:  host*, freq_hz*, port, password
#   soapy: driver*, freq_hz*, rate_hz*, gain_db   (build with --features soapy)
#   hpsdr: host*, freq_hz*, rate_hz*, port        (build with --features hpsdr)
# Any source flag on the command line (--device, --source, --kiwi-host,
# --soapy-driver, --hpsdr-host) replaces a typed [input] table whole,
# its shared keys below included. Example:
# type = "kiwi"               # audio | file | kiwi | soapy | hpsdr
# host = "<your-kiwi-host>"   # kiwi, hpsdr
# port = 8073                 # kiwi (default 8073), hpsdr (default 1024)
# freq_hz = 7030000.0         # kiwi, soapy, hpsdr: RF center frequency, Hz
# password = ""               # kiwi (default "")
# The other types' keys:
# device = "USB Audio"        # audio: device-name substring; omit for the default device
# path = "capture.wav"        # file: relative to this file's directory
# iq = false                  # file: true for a raw complex-IQ WAV (--source-iq)
# driver = "driver=rtlsdr"    # soapy: SoapySDR device args
# rate_hz = 192000.0          # soapy, hpsdr: sample rate, Hz
# gain_db = 30.0              # soapy: omit for the device's AGC

# Shared keys, valid with any type or none:

# Per-source oscillator drift correction, ppm; range [-1000, 1000]
# (`manta_spot::calibration_factor_from_ppm`). §1.4, MAN-29. `decode`
# applies it too. --freq-correction-ppm and
# MANTA_INPUT_FREQ_CORRECTION_PPM override it.
freq_correction_ppm = 0.0

# Target post-decimation capture rate, Hz (issue #169). Omit to use the
# source's native rate unchanged. Must evenly divide the source's native
# rate by a power of two, and must itself satisfy fs/93.75 being a power
# of two. manta_dsp::decimate::Decimator, manta_input::DecimatingSource.
# --capture-rate-hz and MANTA_INPUT_CAPTURE_RATE_HZ override it.
# capture_rate_hz = 48000

# Operator-supplied RF dial frequency, Hz, for a source that has no RF
# reference of its own (rig-audio passband). §1.3; MAN-34. Omit for
# sources that report their own tuned frequency. Must be finite and > 0;
# without it, audio-sourced frequencies are baseband offsets, and `run`
# with a [server] table refuses such a source. --dial-freq-hz and
# MANTA_INPUT_CENTER_FREQ_HZ override it.
# center_freq_hz = 14030000

# Fixed replay epoch, Unix seconds: the wall-clock instant a replayed
# file's first sample maps to, in place of the file's mtime (`run` on file
# replay only). --replay-epoch and MANTA_INPUT_REPLAY_EPOCH override it.
# replay_epoch = 1700000000

[spot]
# allowlist, blocklist_path and notch_path are re-read on SIGHUP when the config has a [server] table (MAN-78); cty_path and scp_path need a restart.
# Operator Watch List (§6, MAN-28): callsigns here bypass grammar/cty
# validation and the repetition gate entirely in manta-spot's validator.
# A non-empty --allowlist replaces this list.
allowlist = []
# Bad-callsign file, one callsign per line, `#` comments allowed (MAN-31).
# A relative path resolves against this config file's directory.
# --blocklist overrides it.
# blocklist_path = "bad-calls.txt"
# Notched frequency ranges, one `low_hz-high_hz` per line (MAN-31). Same
# path rule; --notch overrides it.
# notch_path = "notches.txt"
# AD1C's cty.dat replaces the bundled country-prefix table (MAN-79).
# A prefix missing from this file is rejected unless the call is allowlisted.
# Download: https://www.country-files.com/cty/cty.dat
# A relative path resolves against this config file's directory.
# Unset: bundled. --cty overrides it.
# cty_path = "cty.dat"
# MASTER.SCP replaces the bundled known-callsign list. Membership raises
# confidence; absence alone never rejects a call.
# Download: https://www.supercheckpartial.com/MASTER.SCP
# Same path rule. Unset: bundled. --scp overrides it.
# scp_path = "MASTER.SCP"

# [server] key line_format (MAN-88), set inside your real [server] table:
# Which fixed-column wire layout the INBOUND telnet cluster server renders
# each spot line in (§1.4's 0.01 kHz rounding rule, ARCHITECTURE.md §7,
# MAN-88). Accepted values, exhaustive -- an
# unrecognized one is rejected at startup, never silently defaulted:
#   "rbn"     (default) the RBN relay's AK1A layout: frequency to 0.01 kHz
#             ending at column 24, 15-wide callsign column starting at
#             column 27, 6-wide mode field ("CW") at column 42, time at
#             column 71. Matches a live telnet.reversebeacon.net:7000
#             capture byte-for-byte.
#   "skimmer" the CW-Skimmer-native layout: same geometry with the mode
#             field deleted, so everything after the callsign column
#             shifts left by 4 and time lands at column 67. For operators
#             running manta behind W3OA's Aggregator.
# Scoped to the inbound listener only: the outbound `[[rbn_uplink]]`
# client always emits "rbn" regardless of this key. See
# docs/DECISIONS/2026-09-06-man88-ak1a-column-layout.md (Decisions 1-3)
# for the measured column table and that scoping.
# line_format = "rbn"

# [server] keys for station identity (MAN-86), set inside your real
# [server] table: the operator details RBN Aggregator reads out of
# the telnet greeting banner on connect (MAN-86, Aggregator manual v6.0
# §3.1/§9.2; wire format in
# `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md`).
# `station_callsign` is REQUIRED and has no default; the three operator
# keys are optional, and each one is dropped from the banner when absent
# rather than rendered as an empty placeholder. All four are validated at
# deserialize time (a bad value fails daemon start, it is never written to
# the wire): `station_callsign` by `manta_server::config::
# check_operator_callsign` (a base of 3-20 chars of A-Z, 0-9 and `/`, at
# most prefix/base/suffix, one segment a complete callsign -- prefix,
# separating digit, letter suffix -- plus an optional trailing `-N` SSID,
# N = 1-99, sent as `CALL-N-#` (MAN-89); deliberately broader than the
# decoder's `grammar::is_plausible` so real calls such as `JW/LB2PG` and
# `GB3LER/B` start), `operator_name`/`operator_qth` as non-empty free text with no control
# characters (they are interpolated verbatim into every client's banner,
# so an embedded CR/LF would forge cluster lines), `operator_grid` as a 4-
# or 6-character Maidenhead locator.
# station_callsign = "W3XYZ"    # required, no default
# operator_name = "Art"         # optional, default: absent
# operator_qth = "Switzerland"  # optional, default: absent
# operator_grid = "JN46la"      # optional, default: absent

# [server] and [[rbn_uplink]] (manta_server::config) configure the
# telnet/JSON/metrics servers and the RBN uplink; README.md's "Run it as a
# node" shows both. `run` starts the servers only when the resolved config
# has a [server] table.
```

The `[server]` block's normative keys here are `line_format` above
(`"rbn"` default | `"skimmer"`, MAN-88) and the station/operator identity
keys (MAN-86); its further transport keys (listen
addresses, per-IP connection and command budgets) are deployment settings
rather than normative constants of this spec, and
`crates/manta-server/src/config.rs` is their reference.

**Precedence and the environment.** `run`, `soak` and `doctor` resolve
every key as: command-line flag, then `MANTA_<TABLE>_<KEY>` environment
variable (`MANTA_INPUT_FREQ_CORRECTION_PPM` is `input.freq_correction_ppm`;
`<TABLE>` is one of `SERVER`, `INPUT`, `SPOT`, `DETECTOR`, `DECODE`), then
the config file, then the default above. `--config` names the file;
without it they read `MANTA_CONFIG`. An environment value is parsed as a
TOML value (`9300` is an integer, `["W1AW","K1ABC"]` an array), falling
back to a bare string -- except for the string-typed keys, which are
always taken verbatim: `server.station_callsign`, `server.bind_addr`,
`server.metrics_bind_addr`, `server.operator_name`,
`server.operator_qth`, `server.operator_grid`, `input.type`,
`input.device`, `input.path`, `input.host`, `input.password`,
`input.driver`, `spot.blocklist_path`, `spot.notch_path`,
`spot.cty_path`, `spot.scp_path` and
`decode.engine`. Relative paths from the file
resolve against the file's directory; those from a flag or the
environment resolve against the working directory. `[[rbn_uplink]]`
cannot be set from the environment.

Unknown tables, unknown keys in a known table, and unknown `MANTA_*`
variables are errors that name the offender, raised before any source
I/O. `MANTA_GIT_SHA` and `MANTA_FEATURES` are exempt: they are build-time
values of `crates/manta-cli/build.rs`, never read at run time, and
`cargo run`/`cargo test` export both into the processes they start
(MAN-83). `decode` and `oracle`
read only the file `--config` names and never the environment
(`MANTA_CONFIG` included), so their output cannot depend on the ambient
environment; `decode` applies `[detector]`, `[spot]`, `[decode]` and
`input.freq_correction_ppm`, `oracle` applies `[decode]`, and both
validate the whole file.

`SPEC-decode-core-v2.md` §7 adds `[decode]` keys beyond `engine` (`"legacy"`
| `"edge-legacy"` | `"hsmm"`, see that doc's §0): the `EdgeLegacy`/`Hsmm`
evidence/noise/HSMM tunables, additive over this table -- see that
document's §7 for the full v2 key list and defaults. All of them are
parsed by `manta_decode::config_file::DecodeConfigToml`, and every command
that takes `--config` (`run`, `soak`, `doctor`, `decode`, `oracle`) reads
`[decode]` through it. The full decision record is
`docs/DECISIONS/2026-10-06-man261-config-surface.md`.

`[server]`'s remaining keys are operational limits, not decode-core
constants: listener ports and the per-listener connection/rate quotas
(MAN-57/MAN-61) are documented on `manta_server::config::ServerConfig`'s
own fields, with the exposure policy in ARCHITECTURE §7.

## 10. Deviations from ARCHITECTURE.md

1. **Kaiser prototype is new code, not reused** (§1.2): `coppa-dsp::filter`
   only ships `RrcFilter`; the reuse table's "FIR design → coppa-dsp::filter"
   row is wrong for the PFB prototype. `FftProcessor` reuse stands.
2. **`AdaptiveAgc` dropped from the decode path** (§3.1): the dual-EMA
   threshold is self-normalizing; coppa's block AGC adds latency and couples
   with keying. Replaced by a per-track fixed reference scale.
3. **Beam search is character-local** (§4.4): resets at character boundaries;
   greedy across characters. Word-level context belongs to the validator.
4. Stopband target tightened from the implied ~60 dB to **80 dB** (§1.2) —
   free given 8 taps/branch, and pileup scenes (V8) have ≥ 27 dB dynamic
   range between neighbors.
5. **`[detector] on_snr_db` defaults to 12.0, not 6.0** (§2.3, §9): raised
   by `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 2 --
   at 6.0 dB the channelizer's autocorrelated per-hop noise produced 298
   spurious ACTIVE tracks on V1.
6. **Keying threshold is an additive band about the linear-amplitude
   midpoint, not a geometric-mean threshold with multiplicative hysteresis**
   (§3.2/§3.3, MAN-103): ARCHITECTURE §5's "adaptive threshold at their
   geometric mean" text is superseded — see
   `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`.
