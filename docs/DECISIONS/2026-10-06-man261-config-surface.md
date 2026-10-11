# 2026-10-06 — MAN-261: one TOML file drives a manta deployment

**Status:** Implemented (branch `MAN-261`). Records the design decisions
behind the config surface that `run`, `soak` and `doctor` (and, without the
environment tier, `decode` and `oracle`) now share. Replaces the canceled
MAN-229 and MAN-74 attempts.

## Context

Before this change `manta run --config <file>` read only `[server]` and
`[[rbn_uplink]]` (plus `[decode]` through a second, permissive parse).
Source selection, the KiwiSDR/SoapySDR/HPSDR options, `--dial-freq-hz`,
`--capture-rate-hz`, `--source-iq`, `--freq-correction-ppm`, the
allow/block/notch lists and every detector tunable were flag-only, and any
other table in the file was silently ignored: a `[detectr]` typo, an
`[input]` table naming a KiwiSDR, and an out-of-range
`input.freq_correction_ppm = 999999` all loaded without a word. SPEC §9's
key block was not valid TOML (two pairs per line), listed `on_snr_db =
6.0` against a code default of 12.0, and listed eight keys the code
hard-codes.

## Decisions

- **D1 — module layout.** The loader lives in a new
  `crates/manta-cli/src/config.rs`: read bytes, UTF-8, strip a BOM, parse
  one `toml::Table`, check the top level, apply the environment overlay,
  then deserialize each table from a clone. `[detector]` parsing lives
  next to the type it builds, in `manta_engine::config_file::
  DetectorConfigToml` (mirroring `manta_decode::config_file`), with an
  exhaustive struct literal so a new `DetectorConfig` field fails to
  compile until it gets a key. `DaemonConfigFile` and `DecodeConfigFile`
  keep their public, permissive shape (open PR #95 parses the former);
  strictness is decided once, in `manta-cli`, the only crate that knows the
  full table set. `LiveSourceSpec` takes open PR #207's (MAN-73) name and
  variant shape so the reconnect work rebases onto it mechanically.
- **D2 — strict top level.** The known tables are `[server]`,
  `[[rbn_uplink]]`, `[input]`, `[spot]`, `[detector]` and `[decode]`. An
  unknown table is an error naming it, with a `did you mean [detector]?`
  suggestion within edit distance 2. A top-level scalar, `[[input]]`
  (multiple sources), a plain `[rbn_uplink]` table, and `[[rbn_uplink]]`
  without `[server]` are each a specific error.
- **D3 — SPEC §9's eight constant-only keys are rejected by name.**
  `floor_quantile`, `floor_window_ms`, `block_channels`,
  `block_allowance_db` (`manta-dsp::floor`) and `mu_ratio_bounds`,
  `char_gap_dits`, `word_gap_dits`, `cluster_alpha` (`manta-decode::timing`)
  give `detector.floor_quantile is not configurable yet: it is a
  compile-time constant in manta-dsp::floor …` rather than a generic
  unknown-field error, because SPEC listed them.
- **D4 — `[detector]` keys and bounds.** `on_snr_db`, `off_snr_db`,
  `confirm_ms`, `hang_ms`, `gc_ms`, `warmup_ms`, `track_cap`,
  `silent_respawn_cooldown_ms`, each optional and overlaid on
  `DetectorConfig::default()`; milliseconds convert with
  `manta_decode::ms_to_hops`. `0 <= off_snr_db <= on_snr_db <= 100`, every
  `*_ms` finite in `[0, 3600000]`, `confirm_ms`/`hang_ms`/`gc_ms` at least
  one hop, `track_cap >= 1` — exactly the values the track manager treats
  as degenerate. `DetectorConfig` gains `PartialEq`.
- **D5 — `[input]` is one flat `deny_unknown_fields` table with an
  optional `type`.** `audio` (`device`), `file` (`path`, `iq`), `kiwi`
  (`host`, `port`, `freq_hz`, `password`), `soapy` (`driver`, `freq_hz`,
  `rate_hz`, `gain_db`), `hpsdr` (`host`, `port`, `freq_hz`, `rate_hz`),
  plus the shared `freq_correction_ppm`, `center_freq_hz`,
  `capture_rate_hz` and `replay_epoch`, valid with any type or none. A
  source key without a `type`, a key for another type, and a missing
  required key are named errors. Values go through the same `check_*`
  validators the flags use, so the messages match (scenario 3:
  `freq_correction_ppm 999999 is outside the supported range [-1000,
  1000]`). The hpsdr bounds are un-gated so the table validates the same on
  every build; feature availability is checked only when the source is
  opened.
- **D6 — precedence.** CLI flag, then environment, then file, then default.
  Source selection is all-or-nothing: any CLI source selector (`--device`,
  `--source`, `--kiwi-host`, `--soapy-driver`, `--hpsdr-host`) discards a
  *typed* `[input]` table as a unit, shared keys included, with a stderr
  note; an untyped `[input]` still applies. `--source-iq` alone sets `iq`
  on a config `type = "file"` source. The shared scalars merge key by key;
  `--freq-correction-ppm` became `Option<f64>` so an explicit `0` beats a
  file's `2.5`. A non-empty `--allowlist` replaces the file's list;
  `--blocklist`/`--notch` beat `blocklist_path`/`notch_path`. Relative
  paths from the file resolve against the file's directory, from a flag or
  the environment against the working directory.
- **D7 — which command reads what; servers iff `[server]`.** `run` applies
  all six tables and starts the servers and uplinks if and only if the
  resolved config has a `[server]` table (from the file or
  `MANTA_SERVER_*`). `soak` and `doctor` gain `--config` and apply
  `[input]`, `[spot]`, `[detector]` and `[decode]`, never starting servers.
  `decode` applies `[detector]`, `[spot]`, `[decode]` and
  `input.freq_correction_ppm`; `oracle` applies `[decode]`; both validate
  the whole file and note what they ignore. The dial guard (`--dial-freq-hz
  is required with --config …`) runs after the load and before any source
  I/O, and is satisfied by `center_freq_hz` from any tier.
  `start_spot_server` takes parsed structs instead of re-reading the file.
  *Superseded for doctor by MAN-126 D10:* `doctor` now reads `[server]` and
  `[[rbn_uplink]]` to check their ports and targets, still starting no
  server and logging in nowhere; see
  [2026-10-10-man126-doctor-setup-checks](2026-10-10-man126-doctor-setup-checks.md).
- **D8 — environment tier (`run`, `soak`, `doctor` only).** `MANTA_CONFIG`
  is the fallback for `--config`. `MANTA_<SERVER|INPUT|SPOT|DETECTOR|
  DECODE>_<KEY>` overlays the parsed document before typed parsing; values
  are parsed as TOML with a bare-string fallback, except the string-typed
  keys (`server.station_callsign`, `server.bind_addr`, `input.type`,
  `input.device`, `input.path`, `input.host`, `input.password`,
  `input.driver`, `spot.blocklist_path`, `spot.notch_path`,
  `decode.engine`), which are taken verbatim so a numeric-looking password
  stays a string. An empty value is unset. `MANTA_RBN_UPLINK_*` and any
  other unknown `MANTA_*` name are errors; `MANTA_GIT_SHA` is exempt
  because `crates/manta-cli/build.rs` reads it at build time. Typed-parse
  errors name the contributing variables. The environment is read with
  `vars_os()`, so an unrelated non-UTF-8 variable is skipped and a
  non-UTF-8 `MANTA_*` value is a named error. `decode` and `oracle` never
  read the environment, `MANTA_CONFIG` included: they are the
  deterministic golden-vector and measurement tools.
- **D9 — no new argv scan.** `crates/manta-cli/src` never calls
  `std::env::args()` or `std::env::vars()`; see "Scenario 4" below.
- **D10 — flag-to-key completeness is a test.** A `FLAG_KEYS` table maps
  every config-backed clap argument to its key; one unit test walks
  `Cli::command()` for `run`, `soak`, `doctor` and `decode` and fails on any
  argument that is neither mapped nor in the CLI-only list (`json`,
  `duration`, `config`, `path`, `help`, `version`), and another asserts the
  loader accepts every mapped key. A future flag without a config key fails
  CI.

## Scenario 4 (non-UTF-8 argv panic) did not reproduce on `main`

The ticket's fourth scenario (a non-UTF-8 argument panicking the binary)
came from MAN-74's branch, which scanned `std::env::args()`. On `main`
the only argv scan, `warn_deprecations()`, already reads
`std::env::args_os()` with `to_string_lossy`, and `Cli::parse()` turns
invalid UTF-8 in a `String` argument into a clap error (exit 2). Every
probe (a 0xFF byte in the `--source` path, the `--config` path, the
`--server-config` path, `--kiwi-host`, and an unrelated environment
variable) exited with an error, never a panic. The scenario is therefore
guarded rather than fixed: end-to-end tests assert exit 1 and no
`panicked` for each case plus a non-UTF-8 `MANTA_*` value, and D9's grep
rule keeps `args()`/`vars()` out of the crate.

## Migration notes

1. `run --config` with no `[server]` table now decodes without servers and
   prints a note. It used to fail late, after the source opened, with
   `missing field server`.
2. Unknown tables, top-level keys, unknown keys in known tables and the
   constant-only SPEC keys are errors; they used to be ignored. SPEC §9's
   old block was not valid TOML, so no deployment copied it verbatim.
3. `[input]`, `[spot]` and `[detector]` now take effect wherever they were
   written, including `decode --config` for `[detector]`, `[spot]` and
   `input.freq_correction_ppm`.
4. `run`, `soak` and `doctor` read `MANTA_*` variables; an unknown one is
   an error.
5. The missing-file error reads `reading config file <path>`, not
   `reading --server-config <path>`.
6. `--freq-correction-ppm` no longer shows clap's `[default: 0]`; its help
   names the default and the config key.
7. Unchanged: `--server-config` remains a hidden, deprecated alias on
   `run`; `DaemonConfigFile` and `DecodeConfigFile` keep their public
   shape.

## Follow-ups (not in this change)

- Make the eight constant-only SPEC §9 keys configurable (floor estimator
  and timing constants).
- Multi-source `[[input]]` (MAN-13), repeating `LiveSourceSpec`.
- Reconnecting sources (MAN-73, PR #207) on top of `LiveSourceSpec`.
- `manta config check` / `manta config init`, SIGHUP reload, per-listener
  bind addresses.
- Value validators for `--kiwi-freq` and `--soapy-freq`/`-rate`/`-gain`
  matching the config path's checks, after PR #138's renames land.
- `decode`'s `no signal found (input shorter than one filter length or
  empty)` is misleading when the input was long enough but no track was
  promoted (seen with `[detector] on_snr_db = 99`).
- Route `manta status` (PR #95) through `config::load` if #95 lands after
  this change.

## References

- Ticket: MAN-261 (replaces MAN-229 and MAN-74; MAN-74's branch is
  `archive/MAN-74-pre-replan`)
- Research and plan: the thoughts pool's
  `2026-10-06-MAN-261-operators-should-be-able-to-run-a-24-7-manta-deployment`
  documents
- `docs/SPEC-decode-core.md` §9 (the key table) and §10 item 5
- `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 2
  (`on_snr_db` 12.0)
- `ARCHITECTURE.md` §8, `README.md` "Run it as a node"
