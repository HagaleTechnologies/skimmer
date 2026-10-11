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

No release has been tagged yet. Changes before this file started are in the
git history and `docs/DECISIONS/`.

## [Unreleased]

### Added

- `manta doctor` checks the whole setup before its signal check: the config
  (`manta run`'s example-callsign and dial-frequency refusals), the audio
  library and sound-card inputs, that each `[server]` port is free, the
  clock against an NTP server (and, on Linux, the kernel's NTP sync), and
  that each enabled `[[rbn_uplink]]` target accepts a connection. Each
  check prints `PASS`, `WARN`, `FAIL` or `SKIP`, every problem with a
  `fix:` line, followed by a summary naming each failed and warned check
  (MAN-126). See `docs/RUNBOOKS/setup-checks.md`.
- `manta doctor --ntp-server HOST[:PORT]` names the clock check's server
  (default `pool.ntp.org`).
- `manta doctor --json` adds `checks` and `checks_status`; every existing
  key is unchanged.
- `manta doctor` exits 1 when any check fails. Scripts may now see exit 1
  for a held port, an unreachable uplink, a clock 60 s or more off, the
  example callsign, or a missing dial frequency with `[server]`. The signal
  verdict still does not change the exit code.

### Changed

- `manta doctor` no longer prints `note: doctor does not start the spot
  servers; ignoring [server]`: it checks `[server]`'s ports instead, still
  starting no server.
- A receiver that `manta doctor` cannot open is a named `FAIL  receiver:`
  line with a fix rather than an `Error:` chain (still exit 1). With
  `--json`, doctor then prints an object holding only `checks` and
  `checks_status` instead of nothing.

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
