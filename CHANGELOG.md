# Changelog

Notable changes to manta. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) plus one rule for
decoder output.

**Decoder output rule.** Decoder output is the set of spots manta produces
from a given input file and configuration: which calls are spotted, when, and
at what frequency, SNR, speed, spot type and confidence. That is what
`manta decode --json` and the spot lines of `manta run --json` record. A
change that alters it is listed under `### Decoder output` in
`## [Unreleased]`, and the next release must then bump at least the MINOR
version. A PATCH release never changes decoder output. The JSON stream's
`decoderVersion` (`manta-<version>+<commit>`) is build identity, not decoder
output. See
[docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md](docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md).

`0.1.0` is the first tagged release; its GitHub Release notes list every
pull request merged before it. Changes before this file started are also in
the git history and `docs/DECISIONS/`.

## [Unreleased]

## [0.1.0] - 2026-10-11

Pre-stability alpha, expect breakage: manta has not cleared its own M2/M3
acceptance gates.

### Added

- Prebuilt archives for Linux (x86-64, arm64), macOS (x86-64, arm64) and
  Windows (x86-64) on each GitHub Release, with `SHA256SUMS` and a
  build-provenance attestation (MAN-298).
- `manta run` takes `-v`/`-vv` and `-q`/`-qq`/`-qqq` and
  `--log-level <LEVEL>`, each shorthand for `RUST_LOG=<level>` that
  overrides it, and `--log-format json`, which writes every stderr log line
  as one JSON object for a log aggregator (MAN-124).

### Fixed

- `manta run`'s log lines carry no colour escape codes when stderr is not a
  terminal (a file, a pipe, the journal or a container log); `NO_COLOR` is
  still honoured on a terminal (MAN-124).

### Changed

- `manta --version` (and `-V`) names the exact build: crate version, git
  commit and compiled-in features, for example
  `manta 0.1.0 (git 1a2b3c4d5e6f; features: hpsdr)` (MAN-83).
- The JSON spot stream's `decoderVersion` carries the git commit as SemVer
  build metadata, for example `manta-0.1.0+1a2b3c4d5e6f`, instead of
  `manta-0.1.0` for every build (MAN-83). Decoder output is unchanged.
- `manta run` prints decoded text on stderr, one labelled line per track,
  instead of interleaving every track's characters on stdout. `SPOT:` lines
  move from stderr to stdout. With a `[server]` table decoded text is off
  unless `--decoded-text` is given (MAN-123). Decoder output is unchanged.
