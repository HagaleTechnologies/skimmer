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

- `manta decode --center-freq-hz HZ` sets the recording's RF centre
  frequency, overriding its `<name>.json` sidecar. Without the flag, a
  recording with no sidecar (or a sidecar whose `center_freq_hz` is not
  positive) gets a warning that its frequencies are baseband offsets
  (MAN-131).

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

### Fixed

- Live sound-card input no longer ends `run`, `listen`, `soak` or `doctor`
  when the capture buffer is momentarily empty; a device that delivers
  nothing for 5 s is reported as an error. Audio device errors name the
  device and the 48000 Hz requirement and say what to check next, with a
  macOS microphone-permission hint, and a device delivering only digital
  silence gets a warning (MAN-131). Decoder output is unchanged.
