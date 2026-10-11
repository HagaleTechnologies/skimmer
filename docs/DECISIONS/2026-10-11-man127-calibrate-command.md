# MAN-127: `manta calibrate` measures the receiver's frequency offset

MAN-29 shipped the correction knob: `--freq-correction-ppm`,
`[input].freq_correction_ppm` and `MANTA_INPUT_FREQ_CORRECTION_PPM` each scale
reported frequencies by `1 + ppm × 10⁻⁶`. Nothing measured the right value;
operators guessed or did the field-node runbook's `shadow-compare` arithmetic
by hand. `manta calibrate` opens the configured receiver the way `run` and
`doctor` do, measures a carrier whose frequency is known exactly, reports the
`freq_correction_ppm` that cancels the receiver's error with the evidence
behind it, and saves that value to the config file once the operator confirms.

The correction keeps MAN-29's meaning: it scales reported frequencies only. It
never retunes the source, calls SoapySDR `setFrequencyCorrection` or touches a
KiwiSDR's GPS correction. Raw IQ never carries it, so a measurement taken from
the source is absolute, and a new value replaces the old one rather than
adding to it.

## Decisions

1. **Layers.** `manta-dsp::carrier` is a pure, streaming, deterministic
   narrowband estimator with no policy beyond two exported constants.
   `manta-engine::calibrate` holds the reference catalogue, passband
   planning, the bounded read loop, the gates and the report.
   `manta-cli`'s `config_edit.rs` does the single-key TOML edit and the safe
   replace; `calibrate_cmd.rs` is the command: flags, prompt, rendering and
   exit codes.
2. **Method.** For each planned reference, the estimator mixes the IQ by
   `−offset` (offset = reference − reported centre) with an f64 phase
   accumulator, then decimates by 2 with the repo's halfband stages while
   `0.2 · fs ≥ W`, W being the search half-width. It cuts the decimated
   stream into one-second segments (`round(fs_decimated)` samples), applies a
   Hann window and an FFT, and takes the strongest bin within ±W. The
   ascending scan uses strict `>`, so ties go to the lowest bin.
   `channelizer::interpolate_offset` gives the sub-bin offset. A source
   discontinuity drops the partial segment and resets the stages. The
   measured frequency is the reference plus the **median** offset of the
   valid segments, in the receiver's own frame: centre frequency as the
   source reports it, sample rate as nominal. The deprecated `freqest` is not
   revived.
3. **Gates.** A segment is valid when its peak bin is at least 15 dB over the
   median bin power of the search window; noise alone reaches about 10 dB,
   so 15 dB keeps the per-segment false-detection rate below 10⁻⁶. A
   reference needs at least 10 valid segments, otherwise its status is
   `too_few_segments`. In the power spectrum averaged over all segments, the
   strongest bin must beat the strongest bin more than 25 Hz away from it by
   at least 6 dB, otherwise the status is `ambiguous`. The 25 Hz exclusion,
   rather than a few bins, comes from the prototype: keying sidebands pulled
   a keyed beacon's margin down to 7.6 dB with a ±5-bin exclusion. The
   result is the first reference, in plan order, whose status is `measured`;
   the others print as cross-checks.
4. **Report numbers.** `(±x ppm)` is the standard error of the median,
   `1.2533 × spread / √valid`, never printed below 0.01 ppm. `spread` is
   1.4826 × the median absolute deviation of the valid offsets, in Hz; over
   5 Hz prints a warning that the receiver may still be warming up or that
   another signal shares the window. "dB over the noise floor" is the median
   over valid segments of peak-bin ÷ median-bin power in a ~1 Hz bin. It is
   **not** the pipeline's SNR in 2500 Hz: 15 dB here is about −20 dB SNR in
   2500 Hz. Frequencies print in Hz with one decimal, because kHz rounding
   would erase the sub-kHz error the command exists to resolve. This is a
   value-inspection exception like `config check`'s.
5. **Sign.** MAN-29 applies `f_reported = f_measured × (1 + ppm × 10⁻⁶)`
   (`manta_spot::validator::calibration_factor_from_ppm`). Requiring
   `f_reference = f_measured × (1 + ppm × 10⁻⁶)` gives

   ```text
   ppm = (reference / measured − 1) × 10⁶
   ```

   exactly, not to first order. A receiver that reads 14.2 Hz high at
   10 MHz gives −1.42. To first order this equals the field-node runbook's
   `−Δf / f × 10⁶` and the `--freq-correction-ppm` help's "about 20 Hz high
   on 14 MHz → roughly −1.4". The result passes through
   `calibration_factor_from_ppm` as a range guard (|ppm| ≤ 1000).
6. **Saved value.** The saved and displayed value is rounded to 0.01 ppm,
   with `-0.0` normalised to `0.0`. That is 0.1 Hz at 10 MHz, below the
   estimator's error on every prototype case except fading. Per-reference
   JSON values stay raw.
7. **No CHU, no user list.** The catalogue is fixed (below). CHU is excluded
   because it shut down on 2026-06-22. `--reference-hz` replaces the
   catalogue with one `operator` reference for any other carrier the
   operator knows to a fraction of a hertz. The planning rule applies to it
   unchanged.
8. **NCDXF beacons are automatic references, ranked last.** No public source
   states their transmitter accuracy, so they are labelled `NCDXF beacon` in
   the report and planned after every time standard. They share each
   frequency in 10-second slots, so the help suggests `--duration 180`.
9. **Save rules.** Calibrate saves only `[input].freq_correction_ppm`, and
   only into the config file it read (`--config`, else `MANTA_CONFIG`).
   - On a terminal (stdin and stderr both terminals) it asks
     `Save freq_correction_ppm = <v> to [input] in <path>? [y/N]` on stderr.
     Only `y` or `yes`, case-insensitive and trimmed, saves; anything else,
     including end of input, prints `not saved: answered no` and exits 0.
   - Without a terminal it never saves unless `--write` is given. A piped
     `y` is not confirmation. `--json` never prompts and saves only with
     `--write`.
   - `--write` is refused before any receiver is opened when there is no
     config file, or when a source-selector flag (`--source`, `--device`,
     `--kiwi-host`, and so on) replaced the file's `[input]`: that
     measurement describes a different receiver. `--tune-hz` is the way to
     point the configured receiver at a reference. The same refusal applies
     when the file's `[input]` has a `type` and `MANTA_INPUT_TYPE`,
     `_DEVICE`, `_PATH`, `_HOST`, `_PORT` or `_DRIVER` is set, because that
     variable picks the receiver the way a selector flag does, even when it
     repeats the file's own value.
   - When the file already holds the rounded value, it says so and writes
     nothing.
   - When `MANTA_INPUT_FREQ_CORRECTION_PPM` is set, it warns that the
     variable overrides the saved value for `run`, `soak` and `doctor`. It
     still saves, and never writes environment variables or creates a
     config file.
10. **Safe edit.** `config_edit.rs` uses `toml_edit` 0.25, already in the
    lock graph through `proc-macro-crate`, so `Cargo.lock` gains only two
    dependency-list lines. Every other byte is preserved: comments, order,
    and an existing value's trailing comment, whose decor is copied because
    a plain assignment drops it. A missing `[input]` is appended; an inline
    `input = { ... }` or dotted `input.*` keys are edited in their own style;
    a UTF-8 BOM is re-prepended. The file is replaced atomically:
    canonicalize, so a symlinked config is edited at its target and the link
    stays a link; write a `create_new` temp file in the same directory and
    `sync_all` it; copy the mode; on Unix, `chown` to the original uid and
    gid when they differ; validate the temp file with `config::load`; then
    rename it over the target. If the owner cannot be kept, the save fails
    and names the value to set by hand. A drop guard removes the temp file
    on every error path, and an invalid file is left byte-identical.
11. **Refusals before I/O.** These are refused before any receiver is
    opened, with exit 1: `--duration` outside 10 to 3600 s; `--tune-hz` on a
    sound card or recording; an audio or file source with no
    `--dial-freq-hz` and no absolute frequency; and no usable reference in
    the passband. The last one suggests `--tune-hz` values. A failed
    measurement still prints its report (each reference says why it was not
    measured), then exits 1.
12. **Output conventions.** Calibrate reads no `cty.dat`, so it prints no
    stale-`cty.dat` warning. It prints one plain stderr line,
    `measuring <ref> for <n> s`, before the wait, because a silent 60-second
    wait looks like a hang.

## Constants

These are fixed, not configurable.

| Constant | Value |
|---|---|
| `SEGMENT_S` | 1.0 s (segment = `round(fs_decimated)` samples) |
| `MIN_SEGMENT_PEAK_OVER_FLOOR_DB` | 15.0 |
| `MIN_VALID_SEGMENTS` | 10 |
| `AMBIGUITY_EXCLUSION_HZ` | 25.0 |
| `MIN_AMBIGUITY_MARGIN_DB` | 6.0 |
| `MIN_HALF_WIDTH_HZ` | 100.0 |
| `PASSBAND_EDGE_FRACTION` | 0.05 (use the inner 90 % of `rf_passband_hz`) |
| `DC_GUARD_HZ` | 25.0 |
| `SPREAD_WARN_HZ` | 5.0 |
| `MAX_REFERENCES` | 3 |
| `--duration` | default 60 s, range 10 to 3600 s |
| `--search-ppm` | default 50, range 1 to 1000 |
| Live-source stall deadline | `duration + 10 s` wall clock, checked between reads |

## Reference catalogue

`manta_engine::calibrate::REFERENCES`:

| Hz | Kind | Label |
|---|---|---|
| 2 500 000, 5 000 000, 10 000 000, 15 000 000 | time standard | `time standard (WWV, WWVH, BPM)` |
| 20 000 000, 25 000 000 | time standard | `time standard (WWV)` |
| 4 996 000, 9 996 000, 14 996 000 | time standard | `time standard (RWM)` |
| 14 100 000, 18 110 000, 21 150 000, 24 930 000, 28 200 000 | beacon | `NCDXF beacon` |

Sources, checked 2026-10-10:

- **WWV/WWVH.** WWV transmits on 2.5/5/10/15/20/25 MHz and WWVH on
  2.5/5/10/15 MHz, each with a continuous AM carrier accurate to better than
  2×10⁻¹¹ ([NIST SP 432](https://tf.nist.gov/general/pdf/1778.pdf)). BPM
  shares the 2.5/5/10/15 MHz frequencies.
- **RWM.** RWM transmits on 4.996/9.996/14.996 MHz: an unmodulated carrier for
  minutes 0 to 8 of each half hour, then keyed pulses
  ([sigidwiki RWM](https://sigidwiki.com/wiki/RWM)).
- **NCDXF/IARU beacons.** The beacons use 14.100/18.110/21.150/24.930/28.200
  MHz: 18 beacons in 10-second slots, callsign at 22 WPM, then 1-second dashes
  at 100/10/1/0.1 W ([ncdxf.org/beacon](https://www.ncdxf.org/beacon/)).
  Their oscillator accuracy is unpublished (decision 8).
- **CHU, excluded.** CHU (3.330/7.850/14.670 MHz) shut down on 2026-06-22
  ([NRC](https://nrc.canada.ca/en/certifications-evaluations-standards/canadas-official-time/nrc-shortwave-station-broadcasts-chu),
  [rntfnd.org](https://rntfnd.org/2026/05/22/canada-ending-radio-time-signals-accuracy/)).

## Planning rule

- For each reference, `off = ref − centre`, with the centre as the source
  reports it. `(lo, hi)` is the source's `rf_passband_hz`: Kiwi ±5000 Hz,
  rig audio +300 to +3000 Hz, Nyquist otherwise. `(lo', hi')` is its inner
  90 %, trimming `PASSBAND_EDGE_FRACTION` of the width from each edge.
- The search half-width is
  `W = min(search_ppm · ref · 10⁻⁶, off − lo', hi' − off)`.
- **Centre exclusion.** When `lo < 0 < hi` (an IQ source with its centre in
  band), `W = min(W, |off| − DC_GUARD_HZ)`. A DC spur or birdie at the
  centre would otherwise read as a perfectly calibrated carrier. The repo has
  seen a bin at exactly 10 MHz that could have been a local birdie
  (`docs/DECISIONS/2026-09-10-antenna-path-fix-resolves-detection-gap.md`).
  A reference at the centre is therefore never usable; `--tune-hz` moves it
  1 to 2 kHz off centre.
- A reference is usable when `W ≥ MIN_HALF_WIDTH_HZ`. Usable references are
  ordered time standards first, then beacons, each by descending frequency.
  At most `MAX_REFERENCES` are kept.
- With none usable, the error suggests a `--tune-hz` centre of
  `ref − ceil((W + 1000) / 500) · 500` for the nearest time standard and for
  the 10 MHz one (for example `--tune-hz 9998500`). For audio sources it
  suggests a dial of `ref − 1650`.

## Planning prototype

A throwaway example in `manta-dsp`, removed before implementation, ran the
method's chain: NCO mix, `design_halfband(HALFBAND_TAPS)` decimate-by-2
cascade, 1.365 s Hann FFT segments, `interpolate_offset`, median. It ran on
60 s of 96 kS/s synthetic IQ, centre 9 999 000 Hz, reference 10 MHz,
W = 500 Hz, in a `--release` build:

| Case | True ppm | Estimated | Error | Valid segs | Peak/floor | Ambiguity margin |
|---|---|---|---|---|---|---|
| clean, SNR₂₅₀₀ +10 dB | +2.500 | +2.5007 | +0.0007 | 43/43 | 45 dB | 41.6 dB |
| weak, SNR₂₅₀₀ −15 dB | +2.500 | +2.4995 | −0.0005 | 43/43 | 20 dB | 16.8 dB |
| very weak, −30 dB | +2.500 | none | — | 0/43 | — | — |
| WWV-like AM (500 Hz 50 %, 100 Hz) | −7.300 | −7.2989 | +0.0011 | 43/43 | 34 dB | 11.3 dB |
| NCDXF-like keyed, 1 slot in 3 audible | +1.100 | +1.1001 | +0.0001 | 12/43 | 30 dB | 19.3 dB (25 Hz exclusion; 7.6 dB with ±5 bins) |
| fading + slow Doppler | +0.400 | +0.3866 | −0.0134 | 43/43 | 31 dB | 24.8 dB |
| two carriers 180 Hz apart, −1.9 dB | +2.500 | +2.5006 | — | 43/43 | 35 dB | **2.6 dB → ambiguous** |
| noise only | — | none | — | 0/43 | — | — |

The shipped estimator uses 1-second segments rather than the prototype's
1.365 s, so a 60 s run gives 60 segments. The prototype's unoptimised filter
took about 2.3 s per 60 s of input; the shipped code reuses the repo's
`HalfbandStage`.

## Not done

- No hardware-level correction and no continuous calibration inside `run`
  (for example, tracking NCDXF beacons live).
- No NCDXF slot-schedule decoding, beacon identification or WWV modulation
  check. Spurs are handled only by the centre exclusion and the ambiguity
  gate.
- No new config keys or `MANTA_*` variables, and no `--freq-correction-ppm`
  or spot-filter flags on `calibrate`.
- No live-hardware run: this environment reaches no SDR, KiwiSDR, WWV or
  NCDXF signal. Synthetic-fixture tests cover accuracy (±0.02 ppm), the
  gates, determinism on file input and every save rule.
