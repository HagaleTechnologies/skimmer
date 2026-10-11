//! `manta run`'s diagnostic log (MAN-124): the process's one `tracing`
//! subscriber, the level it filters at and the format it writes to stderr.
//!
//! Level: `-v`/`-q`/`--log-level` when given (each is shorthand for
//! `RUST_LOG=<level>` and overrides it), else `RUST_LOG`, else `info`.
//! Format: text, coloured only when stderr is a terminal and `NO_COLOR` is
//! unset or empty, or JSON Lines for a log aggregator. stderr, never
//! stdout: `run --json` writes JSON Lines there, and any interleaved
//! non-JSON line would corrupt that machine-readable stream and break the
//! byte-identical spot-log requirement (MAN-59 round 6). See
//! docs/DECISIONS/2026-10-10-man124-log-output.md.

use std::ffi::OsStr;
use std::io::IsTerminal as _;
use std::sync::OnceLock;

use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::EnvFilter;

/// `--log-level` values; each is the same single directive `RUST_LOG`
/// would take.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn directive(self) -> &'static str {
        match self {
            LogLevel::Off => "off",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

/// Logging flags for `run`.
#[derive(clap::Args, Clone, Debug, Default)]
#[command(next_help_heading = "Logging")]
pub struct LogOpts {
    /// More log detail: -v is debug, -vv trace. Same as RUST_LOG=debug or
    /// RUST_LOG=trace, and overrides RUST_LOG.
    #[arg(
        short = 'v',
        long = "verbose",
        action = clap::ArgAction::Count,
        conflicts_with_all = ["quiet", "log_level"]
    )]
    pub verbose: u8,
    /// Less log detail: -q is warn, -qq error, -qqq off. Overrides RUST_LOG.
    ///
    /// Text notes, warnings, readiness and reconnect lines are unfiltered.
    /// In JSON format they follow the selected level. Fatal errors always print.
    #[arg(
        short = 'q',
        long = "quiet",
        action = clap::ArgAction::Count,
        conflicts_with = "log_level"
    )]
    pub quiet: u8,
    /// Log level for the log manta writes to stderr. Same as setting
    /// RUST_LOG to this level, and overrides RUST_LOG.
    ///
    /// Text notes, warnings, readiness and reconnect lines are unfiltered.
    /// In JSON format they follow the selected level. Fatal errors always print.
    ///
    /// Without -v, -q or --log-level, RUST_LOG decides (for example
    /// RUST_LOG=info,manta_server=debug), and info is the default.
    #[arg(long, value_enum, value_name = "LEVEL")]
    pub log_level: Option<LogLevel>,
    /// Log line format: `text` (the default), or `json`, one JSON object per
    /// line for a log aggregator.
    ///
    /// Text is colored only when stderr is a terminal and NO_COLOR is unset.
    #[arg(long, value_enum, default_value_t = LogFormat::Text, value_name = "FORMAT")]
    pub log_format: LogFormat,
}

/// Set once `init` has installed the subscriber. Unset (every command but
/// `run`) reads as text, so the helpers below fall back to `eprintln!`.
static FORMAT: OnceLock<LogFormat> = OnceLock::new();

pub fn is_json() -> bool {
    FORMAT.get() == Some(&LogFormat::Json)
}

/// The directive a level flag selects, and which flag chose it. `None`
/// when no level flag was given, so `RUST_LOG` (or `info`) decides.
pub fn flag_directive(opts: &LogOpts) -> Option<(&'static str, &'static str)> {
    if let Some(level) = opts.log_level {
        return Some((level.directive(), "--log-level"));
    }
    match (opts.verbose, opts.quiet) {
        (0, 0) => None,
        (1, _) => Some(("debug", "-v")),
        (_, 0) => Some(("trace", "-v")),
        (_, 1) => Some(("warn", "-q")),
        (_, 2) => Some(("error", "-q")),
        (_, _) => Some(("off", "-q")),
    }
}

/// tracing-subscriber honours NO_COLOR on its own only while `with_ansi`
/// is left unset; calling it, as `subscriber` must for the terminal check,
/// overrides that, so NO_COLOR is checked here too.
pub fn ansi_enabled(stderr_is_terminal: bool, no_color: Option<&OsStr>) -> bool {
    stderr_is_terminal && no_color.is_none_or(OsStr::is_empty)
}

/// The production subscriber, writing to `writer`. Factored out of `init`
/// so tests can capture exactly what `run` would write.
pub fn subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    ansi: bool,
    writer: W,
) -> Box<dyn tracing::Subscriber + Send + Sync>
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter);
    match format {
        LogFormat::Text => Box::new(builder.with_ansi(ansi).finish()),
        LogFormat::Json => Box::new(
            builder
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .finish(),
        ),
    }
}

/// Install `run`'s subscriber. Called once, as the first statement of the
/// `run` arm; every other command logs nothing through `tracing`.
pub fn init(opts: &LogOpts) {
    let (filter, source) = match flag_directive(opts) {
        Some((directive, flag)) => (EnvFilter::new(directive), flag),
        // Today's exact expression: an unset or unparsable RUST_LOG is info.
        None => match EnvFilter::try_from_default_env() {
            Ok(filter) => (filter, "RUST_LOG"),
            Err(_) => (EnvFilter::new("info"), "default"),
        },
    };
    let shown = filter.to_string();
    let ansi = ansi_enabled(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    // `SubscriberInitExt::try_init`, like the `fmt().try_init()` it
    // replaces, also installs the `log` -> `tracing` bridge.
    if subscriber(opts.log_format, filter, ansi, std::io::stderr)
        .try_init()
        .is_ok()
    {
        let _ = FORMAT.set(opts.log_format);
    }
    tracing::debug!(target: "manta", filter = %shown, source, "log filter");
}

/// `main`'s fatal error under `--log-format json`: one ERROR record, written
/// by a JSON subscriber of its own so that the level filter (`-qqq`,
/// `--log-level off`, a target-only `RUST_LOG`) cannot drop it.
pub fn fatal(message: &str) {
    let sub = subscriber(
        LogFormat::Json,
        EnvFilter::new("error"),
        false,
        std::io::stderr,
    );
    tracing::subscriber::with_default(sub, || tracing::error!(target: "manta", "{message}"));
}

/// A `note: ...`-style stderr line: unfiltered text, or a filtered INFO record under
/// `--log-format json`.
pub fn note(line: &str) {
    if is_json() {
        tracing::info!(target: "manta", "{line}")
    } else {
        eprintln!("{line}")
    }
}

/// A `warning: ...`-style stderr line: unfiltered text, or a filtered WARN record
/// under `--log-format json`.
pub fn warning(line: &str) {
    if is_json() {
        tracing::warn!(target: "manta", "{line}")
    } else {
        eprintln!("{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Appends every formatted event into a shared buffer the test reads
    /// back (the `status_line_acceptance.rs` pattern).
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// What the production subscriber writes for the events `f` emits.
    /// Thread-local (`with_default`), so the tests in this binary do not
    /// race on a global subscriber.
    fn capture(format: LogFormat, ansi: bool, f: impl FnOnce()) -> String {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sub = subscriber(format, EnvFilter::new("info"), ansi, Capture(buf.clone()));
        tracing::subscriber::with_default(sub, f);
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn ansi_only_on_a_terminal_without_no_color() {
        assert!(ansi_enabled(true, None));
        // An empty NO_COLOR does not count (no-color.org).
        assert!(ansi_enabled(true, Some(OsStr::new(""))));
        assert!(!ansi_enabled(true, Some(OsStr::new("1"))));
        assert!(!ansi_enabled(false, None));
        assert!(!ansi_enabled(false, Some(OsStr::new(""))));
    }

    #[test]
    fn text_records_carry_escape_codes_only_when_ansi_is_on() {
        let plain = capture(LogFormat::Text, false, || tracing::info!("hello"));
        assert!(
            plain.contains("hello") && !plain.contains('\u{1b}'),
            "{plain:?}"
        );
        let coloured = capture(LogFormat::Text, true, || tracing::info!("hello"));
        assert!(coloured.contains('\u{1b}'), "{coloured:?}");
    }

    #[test]
    fn level_flags_map_to_rust_log_directives() {
        let o = |verbose, quiet, log_level| LogOpts {
            verbose,
            quiet,
            log_level,
            log_format: LogFormat::Text,
        };
        // No flag: RUST_LOG (or info) decides.
        assert_eq!(flag_directive(&o(0, 0, None)), None);
        assert_eq!(flag_directive(&o(1, 0, None)), Some(("debug", "-v")));
        assert_eq!(flag_directive(&o(2, 0, None)), Some(("trace", "-v")));
        assert_eq!(flag_directive(&o(5, 0, None)), Some(("trace", "-v")));
        assert_eq!(flag_directive(&o(0, 1, None)), Some(("warn", "-q")));
        assert_eq!(flag_directive(&o(0, 2, None)), Some(("error", "-q")));
        assert_eq!(flag_directive(&o(0, 3, None)), Some(("off", "-q")));
        assert_eq!(flag_directive(&o(0, 9, None)), Some(("off", "-q")));
        assert_eq!(
            flag_directive(&o(0, 0, Some(LogLevel::Warn))),
            Some(("warn", "--log-level"))
        );
    }

    #[test]
    fn json_records_are_flat_and_carry_the_current_span() {
        // ansi=true must not leak into JSON.
        let out = capture(LogFormat::Json, true, || {
            let span = tracing::info_span!("telnet_client", peer = "127.0.0.1:5000");
            let _g = span.enter();
            tracing::info!(ip = "203.0.113.7", "connect");
        });
        assert!(!out.contains('\u{1b}'), "{out:?}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "{out}");
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["message"], "connect");
        assert_eq!(v["ip"], "203.0.113.7");
        assert!(v["timestamp"].is_string() && v["target"].is_string(), "{v}");
        assert_eq!(v["span"]["name"], "telnet_client");
        assert_eq!(v["span"]["peer"], "127.0.0.1:5000");
        assert!(v.get("fields").is_none() && v.get("spans").is_none(), "{v}");
    }
}
