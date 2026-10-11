# manta

Open-source, cross-platform, wideband multi-signal CW skimmer in Rust. Decodes
every CW signal across an SDR passband and emits RBN-compatible spots — a
second, independent implementation alongside CW Skimmer Server, for the
Linux/ARM/headless platforms it has no native build for.

## Status

M1 implemented (live audio decode; manual W1AW live-copy run still
outstanding). All M2 sub-projects implemented (PFB channelizer;
detector/track manager + decoder pool; V8/V8w pileup + CPU-budget bench;
SoapySDR input; KiwiSDR input) — see docs/DECISIONS/2026-07-1[7-9]*.md and
2026-07-2[4-5]*.md. V1/V3/V4/V7/V8/V9/V10 green; V2's WPM gate is green too
(MAN-7/103's near-channel-edge WPM bug is fixed — keying-threshold placement
and a symmetric mark/gap dit-period estimate, see
docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md) — V2's separate
CER gate stays open (SPEC §2.1 warmup-floor dilution, unrelated to WPM).
V6 now passes and V8w meets its 0-bogus criterion with a >= 20/50
validated floor (MAN-213's fade-tracking keying rails, measured 28/50 — see
docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md). **V5's
and the V8w per-signal CER gate's fading-robustness gap is classical-DSP
work to fix before M4** (MAN-107 through MAN-113), not deferred to M4 ML
fusion by design — see docs/DECISIONS/2026-09-06-broad-review-decisions.md
D8. **M2 acceptance
is still open**: Pi4 CPU-budget leg (also paused pending MAN-100 through
MAN-113 landing in full, not just MAN-107-113 above — D6), 24 h live-SDR
soak, and **VR1–VR8** (ROADMAP.md's M2 "Accept when", a standing gate
since 2026-09-09, not a one-time redesign check — 3/9 pass, see below)
are unmet — the first two need physical hardware not reachable from this
environment.
`manta-dsp::single`/`freqest` deprecated in place. `decode.engine` default
is still `legacy`; `hsmm` (MAN-166 decode-core-v2) is implemented and
CLI-reachable but not promoted — stage-2 gate measured FAIL 2026-09-09
(oracle as_word 56%/framed 32% vs required 60%/40%; 4/11 V-vectors, 3/9
VR-vectors pass) — see
docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md. Variable-width
capture (issue #169, `--capture-rate-hz`) implemented -- see
docs/superpowers/specs/2026-09-09-variable-width-capture-design.md.
MAN-261: one TOML file (`[server]`/`[[rbn_uplink]]`/`[input]`/`[spot]`/
`[detector]`/`[decode]`, CLI > `MANTA_*` > file) configures
`run`/`soak`/`doctor` — see docs/DECISIONS/2026-10-06-man261-config-surface.md.
MAN-76: `manta config check` validates a config (run's pre-I/O pipeline, no
source or listener) and `manta config init` writes a commented every-key
scaffold — see docs/DECISIONS/2026-10-07-man76-config-check-init.md.
MAN-96: secondary-skimmer field-node kit (outage counters,
`scripts/field-node.py`, `scripts/shadow-compare.py`, runbook) implemented;
the 30-day hardware run itself is outstanding — see
docs/DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md.
MAN-132: the metrics listener (`/metrics`, `/healthz`) binds
`[server].metrics_bind_addr`, default `127.0.0.1`, while telnet/JSON keep
`bind_addr` (`0.0.0.0`) — see
docs/DECISIONS/2026-10-08-man132-metrics-loopback-bind.md.
MAN-78: SIGHUP to a `[server]` daemon reloads `[spot]` lists and
`[[rbn_uplink]]` `dry_run`; invalid reloads are logged and ignored.
`cty_path`/`scp_path` and every other key need a restart. See
docs/DECISIONS/2026-10-10-man78-live-reload.md.

MAN-79: `--cty`/`--scp` and `[spot].cty_path`/`scp_path` override the
vendored `cty.dat`/`master.scp` at run time. `run`/`listen`/`soak`/`doctor`/
`config check` warn when the built-in `cty.dat` is more than 180 days old.
See docs/DECISIONS/2026-10-10-man79-operator-cty-scp-override.md.
MAN-124: `run`'s log is plain text off a terminal (colour only on a TTY,
`NO_COLOR` honoured); `-v`/`-q`/`--log-level` are shorthand for `RUST_LOG`;
`--log-format json` makes every stderr line a JSON record — see
docs/DECISIONS/2026-10-10-man124-log-output.md.
MAN-116: `manta bench sensitivity` produces the recall/CER-vs-SNR (500 Hz)
table over AWGN/Watterson good/poor × WPM, deterministic per build and
flags — see docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md.

MAN-123: `run`'s plain-text mode prints decoded text on stderr grouped one
line per track and `SPOT:` lines on stdout; with a `[server]` table decoded
text is off unless `--decoded-text` — see
docs/DECISIONS/2026-10-10-man123-grouped-decoded-text.md.

MAN-125: `manta devices` lists audio inputs and optional Soapy selectors;
`manta check [SOURCE]` reports stream rate, input power and passband noise
floor without decoding or services. See
`docs/DECISIONS/2026-10-10-man125-source-diagnostics.md`.

## Documents (read in this order)

- `README.md` — goals and non-goals
- `ARCHITECTURE.md` — 9-crate workspace, data flow, channelizer/decoder/
  validation/output design
- `docs/SPEC-decode-core.md` — implementation-level algorithm spec: exact
  channelizer constants, noise-floor estimator, track state machine, decoder
  equations, confidence formulas, determinism rules, golden test vectors
  V1–V10, and the full config-key table. Implement from this; the design
  decisions are already made.
- `ROADMAP.md` — milestones M0–M4 with acceptance criteria

## Knowledge wiki

`wiki/INDEX.md` is the map of accumulated knowledge — read it before deep
exploration; open pages relevant to your task. After substantive work, run
/wiki-update: distill new gotchas/decisions/corrections into the wiki (or
into docs/ if normative — the wiki points, it never restates). The wiki is
descriptive and always loses conflicts with code and docs/.

## Key constraints

- Reuses `coppa-dsp` (FFT) from the sibling coppa repo. Note: coppa has NO
  Kaiser filter designer — the PFB prototype designer is new code here
  (`manta-dsp::proto`).
- **Watterson upstream fixes landed** (`coppa` main 2026-07-07, commits
  `9ab1547`, `34aec5f`, `fc35895`): the two bugs identified in the
  SPEC-watterson audit (Doppler spread ~41% too fast vs ITU-R F.1487;
  per-block SNR renormalization erasing fading dynamics) are fixed. Golden-
  vector freeze for V4/V5/V8w is **unblocked** — pin the exact coppa
  commit used when the vectors are generated.
  - **Convention verified against coppa's current `watterson.rs`:** the
    fixed convention matches what manta's spec expects — `doppler_spread_hz`
    is the **2σ width** of the Gaussian Doppler PSD (sigma = spread / 2,
    via `doppler_sigma_hz()`), and normalization is **ensemble-only**
    (E|g|² = 1 across realizations; per-realization normalization is
    explicitly rejected in the module doc and code comment). No divergence
    from manta's expected convention.
  - **SNR convention (still live):** this repo's spec froze SNR-in-2500-Hz;
    the shared `awgn_ref_bw()` design in SPEC-watterson reconciles it with
    the benchmark harness's 3 kHz convention. **Superseded at the
    wire-output boundary only** by MAN-102 / decision D3
    (2026-09-06-broad-review-decisions.md): telnet/RBN-uplink spot lines now
    quote SNR in the 500 Hz RBN/CW Skimmer reference bandwidth, converted
    from the pipeline's native 2500 Hz value at render time; the JSON stream
    keeps the native 2500 Hz value plus an explicit `snrRefHz` field. The
    testkit's input-side 2500 Hz convention this bullet otherwise describes,
    and the `awgn_ref_bw()` reconciliation, are unaffected.
- Deterministic decode path is a hard requirement: file input → byte-identical
  spot logs.
- Classical decoder first; ML fusion (dit's pattern) only at M4, gated on
  beating the classical baseline under simulated fading.
- CPU budget: full 192 kS/s passband within one Raspberry Pi 4 core, enforced
  by criterion benches.
- JSON spot schema is an ecosystem contract — belongs in the `dispensa` repo
  (ADR pending), not solely here.
- No GUI: daemon + CLI. Outputs: RBN-format cluster telnet (:7300) and JSON
  Lines/WebSocket (:7301) for cqdx ingest.
- M0 testkit generates its own ref-bandwidth AWGN (see
  docs/DECISIONS/2026-07-11-m0-implementation-pins.md); migrate to coppa
  awgn_ref_bw when it ships.
- coppa pin bumped to `f8a4d16d` for M1 (Watterson fixes; see
  docs/DECISIONS/2026-07-17-m1-implementation-pins.md). `AudioIqSource`
  requires exactly 48000 Hz input — coppa-audio's resampler is unreachable
  (no `mod resampler;`, no `rubato` dep); see that doc's pin 3.

## Multi-agent hygiene

You are never alone in this repo — other agents may be working concurrently
in other clones, branches, or worktrees.

- **Start fresh:** `git fetch` and rebase onto `origin/main` before reading
  code or making decisions; stale context produces wrong work.
- **Claim before work:** search open PRs/issues first; open a draft PR early —
  the draft PR *is* the claim. Don't duplicate in-flight work.
- **Isolate:** always a branch (worktree preferred), never a shared checkout's
  main. Use per-session scratch dirs; don't bind fixed ports.
- **Flush at the end:** push (`--force-with-lease` only) and open/update your
  PR before finishing. Unpushed work is invisible work.
- **Main moves only by PR merge.**
- **Auto-merge is on, repo-wide** (overrides the global "Tony merges"
  default for this repo specifically): every PR gets `gh pr merge --auto
  --squash` right after opening; GitHub merges it unattended once required
  CI (`test (ubuntu-latest)`, `test (macos-latest)`) is green. See
  docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md.

## Code review convergence

Every review round fixes P1 findings inline. From round 2 onward, P2-and-
lower findings are not fixed inline — they're captured verbatim into a
follow-up ticket instead, so the PR converges instead of chasing
progressively finer findings across rounds. Round 1 is unrestricted (fix
everything reasonable). Full policy:
docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md.
