# MAN-123: grouped decoded text, spots on stdout, a quiet daemon

`manta run` (alias `listen`) in plain-text mode used to print every
`CharDecoded` character from every track the moment it arrived. On a
wideband passband that is one unreadable stream. With a `[server]` table the
daemon did the same thing into its service log. Found by the 2026-09-05
broad review, lens 1 #8 and #29, lens 2 #19 (consolidated O-03).

## The problem, reproduced

- **Fifty-station pileup** (`manta_testkit::vectors::v8()`, 96 kHz IQ,
  `run --source v8.wav --source-iq --dial-freq-hz 14000000`): stdout was
  10,756 bytes of interleaved characters (`IA U K FDDDIK J Q K DDT NU Q …`).
  The 53 `SPOT:` lines were on stderr.
- **Same run with a `[server]` table:** stdout was byte-identical to the
  non-daemon run, so the service log received the same decoder confetti.
- **Three stations, 48 kHz mono audio** (W1AW, K9ABC, N0XYZ at 25 dB SNR):
  no callsign appeared intact anywhere in stdout.

## Decisions

1. **One line per track.** `crates/manta-cli/src/text_lines.rs` buffers
   each track's characters separately. A track's line is printed:
   - at the first word gap once the line holds `LINE_MIN_CHARS` = 32
     characters;
   - when it reaches `LINE_MAX_CHARS` = 64 characters with no word gap
     (cut mid-word);
   - when the track's `TrackClosed` arrives;
   - for every still-open track, in `track_id` order, after the decode
     loop returns.

   Measured on the v8 event stream before choosing: one line per word gave
   2,788 lines of 1–5 characters; one line per track close gave 50 lines of
   up to 367 characters that appear only when a track ends; the 32-character
   rule gave 335 lines, at most 65 characters, each about one CQ cycle.
   64 bounds line width and per-track memory (64 characters × `track_cap`).
2. **Text convention.** As `manta_decode::decoder::events_to_text`:
   prosigns are dropped, repeated word gaps collapse to one space, leading
   and trailing gaps are dropped.
3. **Label.** `[track <id> <kHz, 0.1> kHz <WPM, integer> WPM] <text>`, from
   the track's latest `TrackMeta.freq_hz` and `SpeedUpdate.wpm`. A field not
   yet known is left out (`[track 5] CQ`). In about 900 measured lines every
   line had both fields by the time it was printed.
4. **Streams.** In `run`'s text mode, stdout carries `SPOT:` lines only;
   stderr carries decoded text and diagnostics (startup line, tracing
   banner, status, warnings, errors). The `SPOT:` line's wording is
   unchanged; only its stream moved.

   | Mode | stdout | stderr |
   | --- | --- | --- |
   | `run`, no `[server]` table | `SPOT:` lines | decoded text, diagnostics |
   | `run` with a `[server]` table | `SPOT:` lines | diagnostics only |
   | the same plus `--decoded-text` | `SPOT:` lines | decoded text, diagnostics |
   | `--json` (any mode) | events and `{"spot":…}` objects, unchanged | diagnostics |

5. **What counts as a daemon.** A `[server]` table started the servers
   (`spot_server.is_some()` in `main.rs`), the same rule that decides
   whether the telnet/JSON/metrics servers start (D7 of
   2026-09-06-broad-review-decisions.md). `--config` with only `[decode]` or
   `[detector]` tuning stays interactive and prints decoded text.
6. **`--decoded-text`.** A per-invocation flag under the `Output` help
   heading turns decoded text back on in daemon mode. It is accepted and
   harmless outside daemon mode. It conflicts with `--json`, so
   `run --json --decoded-text` is a clap usage error (exit 2) rather than a
   silently ignored flag. There is no config key or `MANTA_*` variable for
   it, as there is none for `--json`: it is a debugging choice. It is on the
   `CLI_ONLY` list of `every_config_backed_flag_maps_to_a_key` (MAN-261 D10).
7. **No state on `TrackPromoted`.** `TrackManager` emits `TrackClosed` only
   for a track that produced decoder output (`has_emitted`, MAN-19). A bare
   promotion never gets a close, so the grouper creates per-track state only
   from `CharDecoded`, `WordBoundary`, `SpeedUpdate` and `TrackMeta`, and
   frees it on `TrackClosed`, as `manta-spot::Validator` does.
8. **Error-path flush.** `listen_with_observers` returns `Err` on a
   mid-stream source read error before `TrackManager::finish()`, so open
   tracks get no `TrackClosed` there. `run` prints every pending line after
   the call returns, on both `Ok` and `Err`, before the existing error
   message.
9. **Determinism.** The grouped text is a pure function of the event order,
   so a file replay prints byte-identical track lines
   (`crates/manta-cli/tests/text_output.rs` runs the fixture twice).

## Rejected alternatives

- **One line per word.** Thousands of one-word fragments; no more readable
  than the old stream.
- **One line per track close only.** Live text stays hidden until a track
  ends, and lines grow to hundreds of characters.
- **An idle or line-age time flush.** Needs sample-rate plumbing into the
  grouper. Track closure already bounds a partial line's wait to the 5 s
  hang (`hang_hops` at 375 Hz hops).
- **A content filter for short junk tracks.** Text mode is a debugging
  view; hiding decoder output would hide exactly what someone debugging
  junk tracks needs. Grouping already isolates junk on its own labelled
  lines.
- **`-v`/`--verbose-text`.** `-v` stays reserved for O-04's log-verbosity
  design.
- **A config-file key.** See decision 6.

## Compatibility

- MAN-270 (PR #231, open when this landed) puts the character stream on
  stderr and spots on stdout too, so its stream table agrees with this one.
  If its `fmt.rs` lands, the grouped lines go through `fmt::monitor_write`
  and the label uses `manta_server::human::{khz, wpm}`; the stream contract
  above stays.
- **Migration:** a script reading `SPOT:` lines from `run`'s stderr must
  read stdout. A script reading decoded characters from stdout must read
  stderr, which now has per-track lines. No consumer in this repo did
  either. Service logs (systemd journal, launchd log file, Docker logs) stop
  receiving decoded text and gain one `SPOT:` line per spot.
