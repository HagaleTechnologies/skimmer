# 2026-10-10 — MAN-124: plain logs off a terminal, verbosity flags, JSON logs

**Status:** Implemented (branch `MAN-124`). Supersedes the "plain text, not
JSON" non-decision in
`docs/DECISIONS/2026-09-03-man59-connection-audit-logging.md`, which
deferred JSON "if/when an operator wants machine-parsed ingestion".

## Context

`manta run` had one `tracing` subscriber, installed inside
`start_spot_server` with `tracing_subscriber::fmt()` and no `with_ansi`.
tracing-subscriber 0.3 colours by default and checks only `NO_COLOR`,
never whether the stream is a terminal, so every redirected log (the
systemd journal, the launchd log file, Docker's `json-file` log) carried
raw `ESC[..m` bytes. The only verbosity control was the undocumented
`RUST_LOG`, and there was no structured output.

## Decisions

- **D1. Colour.** `with_ansi(stderr.is_terminal() && NO_COLOR is unset or
  empty)`, using std `IsTerminal`. Calling `with_ansi` overrides
  tracing-subscriber's own `NO_COLOR` default, so the check is made here.
- **D2. Where the subscriber starts.** `crate::logging::init` is the first
  statement of the `run` arm, for every `run` with or without `[server]`;
  `start_spot_server` no longer installs it. Startup notes and fatal config
  errors then happen after the subscriber exists, so JSON mode covers them.
- **D3. Level.** `--log-level L` is `RUST_LOG=L`; `-v` is debug, `-vv`
  trace; `-q` warn, `-qq` error, `-qqq` off; counts saturate. Any of them
  replaces `RUST_LOG` entirely (CLI over environment, as in MAN-261). With
  none, the old expression stands: `RUST_LOG`, else `info` (an unparsable
  `RUST_LOG` still silently falls back to `info`). The three are mutually
  exclusive. `--log-level` is a closed set, not a free-form directive:
  `RUST_LOG=debgu` (a typo) silently disables every tracing line, because
  `EnvFilter` reads an unknown word as a target name. Free-form filtering
  stays with `RUST_LOG`.
- **D4. A `debug` record naming the filter** (`log filter filter=<directive>
  source=<-v|-q|--log-level|RUST_LOG|default>`) right after init.
- **D5. JSON shape.** tracing-subscriber's JSON formatter with
  `flatten_event(true)`, `with_current_span(true)`, `with_span_list(false)`.
  Every record has `timestamp`, `level`, `message` and `target`; an
  event's fields are top-level keys; an event inside a client connection
  carries `"span":{"name":"telnet_client","peer":"…"}`. A top-level
  `message` is what common aggregators read as the log text without a remap
  rule. These key names come from tracing-subscriber 0.3 and are now an
  operator-facing format: an upgrade that changes them must be called out
  in `CHANGELOG.md`.
- **D6. Plain lines under JSON.** `logging::note` and `logging::warning`
  print their line byte-for-byte, without level filtering, in text mode
  and emit a filtered INFO or WARN record with the same text (prefix
  included) under JSON. Notes, the
  readiness marker and "reconnected after" are `note`; the cty age warning,
  the missing-dial-frequency warning, "lost" and "reconnect attempt failed"
  are `warning`. `soak` and `doctor` share these call sites but never call
  `init`, so their output stays plain.
- **D7. Product output after MAN-123.** `SPOT:` lines stay on stdout in
  both log formats. Enabled decoded text on stderr becomes an INFO record
  with `target: "manta::text"` and the grouped line as `message` under JSON.
  This follows the plan's MAN-123 integration rule.
- **D8. A fatal error under JSON.** `main` wraps `real_main`; an `Err` under
  JSON is one `ERROR` record (message trimmed of trailing newlines) and exit
  status 1. Text mode keeps Rust's `Error: …` line. Under JSON the record is
  written by a JSON subscriber of its own (`logging::fatal`), so the level
  filter (`-qqq`, `--log-level off`, a target-only `RUST_LOG`) cannot drop
  it or turn it back into the plain `Error: …` line. A panic stays one of
  the plain-text exceptions below.

## Reviewer questions, with the defaults built

1. **Config-file or `MANTA_*` keys for level or format?** No: CLI flags plus
   `RUST_LOG` only, listed in `CLI_ONLY` like `--json`. Each packaging
   target can already pass flags, and a `[log]` table would widen MAN-261's
   surface for a choice operators set once.
2. **Should `-q`/`--log-level` hide the plain `note:`/`warning:`/readiness/
   reconnect lines in text mode?** No. They have no level in text mode,
   tests pin their text, and filtering them would make
   `RUST_LOG=manta_server=debug` (MAN-59's recipe) hide them too.
3. **Should `SPOT:` lines become structured log records?** No longer:
   MAN-123 moved them to stdout. See D7.

## Plain-text exceptions under `--log-format json`

clap usage errors and the pre-parse deprecation warning for the retired
`listen`/`--server-config` spellings print before the flags are parsed; a
panic message is printed by the runtime. All three stay plain text.

## Dependencies

Enabling tracing-subscriber's `json` feature adds one crate,
`tracing-serde 0.2.0`. `serde` and `serde_json` were already in the tree.

## Out of scope

Tracing in `manta-engine`/`manta-input`; restructuring the MAN-122 banner,
readiness and status lines into separate JSON fields; `--color`; log files
or rotation (MAN-268); flags on `soak`/`doctor`/`decode`.
