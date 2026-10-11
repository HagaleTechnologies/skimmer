//! MAN-78: live reload of the operator lists (`[spot]`) and each
//! `[[rbn_uplink]]` block's `dry_run`, without a restart. On Unix, SIGHUP to
//! a daemon (a config with a `[server]` table) runs `reload_once` on the
//! `manta-reload` thread; every other setting still needs a restart and is
//! only named in a warning. See docs/DECISIONS/2026-10-10-man78-live-reload.md.
//!
//! Signal handling lives in `spawn` alone, so the rest of this module (and
//! its tests) builds on every OS. Both shutdown cases log at INFO:
//! `reload: SIGHUP ignored; the daemon is shutting down`.
// On Windows nothing outside the tests calls into this module: there is no
// SIGHUP, so no reload.
#![cfg_attr(not(unix), allow(dead_code))]

use crate::{config_cmd, prepare_config, CliOverrides, UplinkDryRun};
use anyhow::Result;
use manta_decode::decoder::Engine;
use manta_engine::{OperatorLists, OperatorListsUpdate};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// D7: closed by `main` once the decode loop has returned and the daemon
/// is draining. `reload_once` holds the lock from its last check through
/// the apply, so a reload either applies before the drain starts or not at
/// all -- never a `dry_run` flip mid-drain.
#[derive(Default)]
pub(crate) struct DrainGate(Mutex<bool>);

impl DrainGate {
    pub(crate) fn close(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    pub(crate) fn is_closed(&self) -> bool {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What a reload needs from startup: how `run` resolved its config, the
/// document it started from, and the live handles it may change.
pub(crate) struct ReloadContext {
    /// `Prepared::config_path` from startup (`--config`, else `MANTA_CONFIG`).
    pub config_path: Option<PathBuf>,
    /// The command line's overrides, so a reload resolves exactly as `run`
    /// did and command-line flags keep winning.
    pub cli: CliOverrides,
    pub cli_engine: Option<Engine>,
    /// Startup environment, reused on reload; tests supply an empty snapshot.
    pub vars: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// `Loaded::raw` at startup: the baseline for restart-only changes.
    pub startup_raw: toml::Table,
    /// Hands new lists to the decode loop.
    pub lists: Arc<OperatorListsUpdate>,
    /// Each `[[rbn_uplink]]` task's live `dry_run` flag, in config order.
    pub uplinks: Vec<UplinkDryRun>,
    pub drain_gate: Arc<DrainGate>,
    /// The same shutdown request flag used by the signal handlers.
    pub stop: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum ReloadOutcome {
    Applied(ReloadReport),
    /// Shutdown began before the apply; nothing changed.
    ShuttingDown,
}

/// What one successful reload applied, for `log_applied`.
#[derive(Debug)]
pub(crate) struct ReloadReport {
    /// Distinct callsigns, uppercased (the validator's own comparison).
    pub allowlist_calls: usize,
    pub blocklist_calls: usize,
    pub blocklist_path: Option<PathBuf>,
    pub notch_ranges: usize,
    pub notch_path: Option<PathBuf>,
    /// `(target label, new dry_run)` for each enabled target whose flag
    /// changed.
    pub dry_run_changes: Vec<(String, bool)>,
    /// An `[[rbn_uplink]]` entry changed beyond `dry_run`, so no `dry_run`
    /// change was applied.
    pub dry_run_skipped: bool,
    /// Changed keys that need a restart, as sorted dotted paths.
    pub restart_needed: Vec<String>,
}

/// Re-runs `run`'s config stage, plus `manta config check`'s own checks,
/// and applies the reloadable parts. All validation runs before anything is
/// applied: an `Err` means nothing changed.
pub(crate) fn reload_once(ctx: &ReloadContext) -> Result<ReloadOutcome> {
    let prepared = prepare_config(
        ctx.cli.clone(),
        ctx.config_path.clone(),
        ctx.cli_engine,
        &ctx.vars,
    )?;
    config_cmd::reject_on_reload(&prepared.loaded, ctx.cli.source_selector().is_some())?;
    let restart_needed = restart_only_changes(&ctx.startup_raw, &prepared.loaded.raw);
    let dry_run_skipped = restart_needed.iter().any(|p| p.starts_with("rbn_uplink"))
        || prepared.loaded.rbn_uplink.len() != ctx.uplinks.len();

    let pipeline = prepared.pipeline;
    let allowlist_calls = pipeline
        .allowlist
        .iter()
        .map(|c| c.to_ascii_uppercase())
        .collect::<BTreeSet<_>>()
        .len();
    let report_lists = (
        pipeline.blocklist.len(),
        pipeline.notch.len(),
        prepared.resolved.spot.blocklist.clone(),
        prepared.resolved.spot.notch.clone(),
    );
    // Held through the apply (D7). Recheck stop after config I/O: a stop
    // signal may have arrived while the source still delays drain entry.
    // All validation is complete; nothing below this check can fail.
    let draining = ctx.drain_gate.0.lock().unwrap_or_else(|e| e.into_inner());
    if *draining || ctx.stop.load(Ordering::Relaxed) {
        return Ok(ReloadOutcome::ShuttingDown);
    }
    ctx.lists.offer(OperatorLists {
        allowlist: pipeline.allowlist,
        blocklist: pipeline.blocklist,
        notch: pipeline.notch,
    });

    let mut dry_run_changes = Vec::new();
    if !dry_run_skipped {
        for (live, new) in ctx.uplinks.iter().zip(&prepared.loaded.rbn_uplink) {
            let old = live.flag.swap(new.dry_run, Ordering::Relaxed);
            // A disabled target runs no task to read the flag
            // (`uplink::serve_with_live_dry_run` returns at once), so
            // nothing starts or stops going out: no change to report.
            if old != new.dry_run && live.enabled {
                dry_run_changes.push((live.label.clone(), new.dry_run));
            }
        }
    }

    drop(draining);

    let (blocklist_calls, notch_ranges, blocklist_path, notch_path) = report_lists;
    Ok(ReloadOutcome::Applied(ReloadReport {
        allowlist_calls,
        blocklist_calls,
        blocklist_path,
        notch_ranges,
        notch_path,
        dry_run_changes,
        dry_run_skipped,
        restart_needed,
    }))
}

/// Every changed key in `new` against `old` that a reload does not apply,
/// as sorted dotted paths (`server.telnet_port`, `rbn_uplink[0].target_port`).
/// Tables recurse, a missing table counting as empty; arrays recurse per
/// index when the lengths match and are reported whole otherwise; other
/// values compare by equality. Only the operator list keys and each
/// `rbn_uplink[N].dry_run` are reloadable and never reported.
pub(crate) fn restart_only_changes(old: &toml::Table, new: &toml::Table) -> Vec<String> {
    let mut out = BTreeSet::new();
    diff_tables("", old, new, &mut out);
    out.into_iter().collect()
}

/// D6: only these four leaf paths reload. In particular, cty/scp tables
/// and uplink spot_types need a restart.
fn is_reloadable(path: &str) -> bool {
    matches!(
        path,
        "spot.allowlist" | "spot.blocklist_path" | "spot.notch_path"
    ) || (path.starts_with("rbn_uplink[") && path.ends_with("].dry_run"))
}

fn diff_tables(prefix: &str, old: &toml::Table, new: &toml::Table, out: &mut BTreeSet<String>) {
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for key in keys {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        diff_values(path, old.get(key), new.get(key), out);
    }
}

fn diff_values(
    path: String,
    old: Option<&toml::Value>,
    new: Option<&toml::Value>,
    out: &mut BTreeSet<String>,
) {
    use toml::Value;
    if is_reloadable(&path) {
        return;
    }
    let empty_table = toml::Table::new();
    let empty_array: Vec<Value> = Vec::new();
    match (old, new) {
        (Some(Value::Table(a)), Some(Value::Table(b))) => diff_tables(&path, a, b, out),
        (None, Some(Value::Table(b))) => diff_tables(&path, &empty_table, b, out),
        (Some(Value::Table(a)), None) => diff_tables(&path, a, &empty_table, out),
        (Some(Value::Array(a)), Some(Value::Array(b))) => diff_arrays(path, a, b, out),
        (None, Some(Value::Array(b))) => diff_arrays(path, &empty_array, b, out),
        (Some(Value::Array(a)), None) => diff_arrays(path, a, &empty_array, out),
        (a, b) => {
            if a != b {
                out.insert(path);
            }
        }
    }
}

fn diff_arrays(path: String, old: &[toml::Value], new: &[toml::Value], out: &mut BTreeSet<String>) {
    if old.len() != new.len() {
        out.insert(path);
        return;
    }
    for (i, (a, b)) in old.iter().zip(new).enumerate() {
        diff_values(format!("{path}[{i}]"), Some(a), Some(b), out);
    }
}

fn display_path(path: &Option<PathBuf>) -> String {
    path.as_deref()
        .map_or_else(|| "none".to_string(), |p: &Path| p.display().to_string())
}

/// The log lines of a successful reload (stderr through `tracing`).
pub(crate) fn log_applied(report: &ReloadReport) {
    tracing::info!(
        allowlist_calls = report.allowlist_calls,
        blocklist_calls = report.blocklist_calls,
        blocklist_path = %display_path(&report.blocklist_path),
        notch_ranges = report.notch_ranges,
        notch_path = %display_path(&report.notch_path),
        "reload: applied"
    );
    // MAN-159's startup convention: `warn` when real spots start going out,
    // `info` for the safe state.
    for (label, dry_run) in &report.dry_run_changes {
        if *dry_run {
            tracing::info!(
                target = %label,
                "reload: dry_run = true -- connected, but NOT transmitting spots to this target."
            );
        } else {
            tracing::warn!(
                target = %label,
                "reload: dry_run = false -- transmitting real spots to this target."
            );
        }
    }
    if !report.restart_needed.is_empty() {
        tracing::warn!(
            "reload: these settings changed but take effect only after a restart; \
             keeping the running values: {}",
            report.restart_needed.join(", ")
        );
    }
    if report.dry_run_skipped {
        tracing::warn!(
            "reload: [[rbn_uplink]] changed beyond dry_run; no dry_run change applied \
             until a restart"
        );
    }
}

/// The log line of a rejected reload: the whole error chain, as `manta
/// config check` would print it.
pub(crate) fn log_rejected(e: &anyhow::Error) {
    tracing::error!("reload: rejected; still running the previous configuration: {e:#}");
}

/// Starts the `manta-reload` thread: one `reload_once` per SIGHUP, until the
/// process exits. Once `stop` is set (a stop signal arrived) or the drain
/// gate is closed (the daemon is draining), a SIGHUP is logged and ignored
/// rather than flipping `dry_run` mid-drain; `reload_once` re-checks both
/// under the drain lock after config I/O, for a SIGHUP past this check.
/// Both paths log INFO: `reload: SIGHUP ignored; the daemon is shutting down`.
#[cfg(unix)]
pub(crate) fn spawn(mut signals: signal_hook::iterator::Signals, ctx: ReloadContext) -> Result<()> {
    use anyhow::Context as _;
    let source = ctx.config_path.as_deref().map_or_else(
        || "the environment".to_string(),
        |p| p.display().to_string(),
    );
    std::thread::Builder::new()
        .name("manta-reload".into())
        .spawn(move || {
            // `main` blocked SIGHUP before starting the source, the servers
            // and this thread, so this is the thread the kernel delivers it
            // to (`mask_sighup`).
            if let Err(e) = mask_sighup(libc::SIG_UNBLOCK) {
                tracing::error!("reload: cannot unblock SIGHUP; reloads are disabled: {e}");
                return;
            }
            for _ in signals.forever() {
                if ctx.stop.load(Ordering::Relaxed) || ctx.drain_gate.is_closed() {
                    tracing::info!("reload: SIGHUP ignored; the daemon is shutting down");
                    continue;
                }
                tracing::info!("reload: SIGHUP received, re-reading {source}");
                match reload_once(&ctx) {
                    Ok(ReloadOutcome::Applied(report)) => log_applied(&report),
                    Ok(ReloadOutcome::ShuttingDown) => {
                        tracing::info!("reload: SIGHUP ignored; the daemon is shutting down");
                    }
                    Err(e) => log_rejected(&e),
                }
            }
        })
        .context("starting the reload thread")?;
    Ok(())
}

/// Blocks (`libc::SIG_BLOCK`) or unblocks (`libc::SIG_UNBLOCK`) SIGHUP for
/// the calling thread only; threads it spawns afterwards inherit the mask.
/// A daemon blocks it on the decode thread before opening the source: a
/// handler run there fails that thread's in-flight socket `recv` with EINTR
/// (a read timeout makes the call non-restartable even under `SA_RESTART`,
/// signal(7)), and the KiwiSDR/HPSDR sources treat that as a lost link.
#[cfg(unix)]
pub(crate) fn mask_sighup(how: libc::c_int) -> std::io::Result<()> {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` initialises `set` before `sigaddset` and
    // `pthread_sigmask` read it, and a null old-set pointer is allowed.
    let rc = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGHUP);
        libc::pthread_sigmask(how, set.as_ptr(), std::ptr::null_mut())
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(rc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    const BASE: &str = "\
[server]
station_callsign = \"W3XYZ\"
bind_addr = \"127.0.0.1\"
telnet_port = 0
json_port = 0
metrics_port = 0

[spot]
allowlist = [\"k5arh\"]
blocklist_path = \"bl.txt\"
";

    fn uplink_block(dry_run: bool, port: u16) -> String {
        format!(
            "\n[[rbn_uplink]]\nenabled = true\ntarget_host = \"127.0.0.1\"\n\
             target_port = {port}\nlogin_callsign = \"W3XYZ\"\ndry_run = {dry_run}\n"
        )
    }

    struct Fixture {
        dir: tempfile::TempDir,
        ctx: ReloadContext,
    }

    impl Fixture {
        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }
    }

    /// A daemon "started" on `config` (written as `manta.toml`, with
    /// `bl.txt` = `K1BAD`), with one live flag per `[[rbn_uplink]]` entry.
    fn start(config: &str, cli: CliOverrides) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("manta.toml");
        write(&config_path, config);
        write(&dir.path().join("bl.txt"), "K1BAD\n");
        let vars = Vec::new();
        let prepared = prepare_config(cli.clone(), Some(config_path.clone()), None, &vars).unwrap();
        let uplinks = manta_server::uplink::target_labels(&prepared.loaded.rbn_uplink)
            .into_iter()
            .zip(&prepared.loaded.rbn_uplink)
            .map(|(label, u)| UplinkDryRun {
                label,
                enabled: u.enabled,
                flag: Arc::new(AtomicBool::new(u.dry_run)),
            })
            .collect();
        Fixture {
            ctx: ReloadContext {
                config_path: Some(config_path),
                cli,
                cli_engine: None,
                vars: Vec::new(),
                startup_raw: prepared.loaded.raw,
                lists: Arc::new(OperatorListsUpdate::default()),
                uplinks,
                drain_gate: Arc::new(DrainGate::default()),
                stop: Arc::new(AtomicBool::new(false)),
            },
            dir,
        }
    }

    fn applied(ctx: &ReloadContext) -> ReloadReport {
        match reload_once(ctx).unwrap() {
            ReloadOutcome::Applied(report) => report,
            ReloadOutcome::ShuttingDown => panic!("reload unexpectedly ignored during shutdown"),
        }
    }

    fn flags(ctx: &ReloadContext) -> Vec<bool> {
        ctx.uplinks
            .iter()
            .map(|u| u.flag.load(Ordering::Relaxed))
            .collect()
    }

    #[test]
    fn valid_edit_offers_new_lists_and_reports_their_sizes() {
        let f = start(BASE, CliOverrides::none());
        write(&f.path("bl.txt"), "K1BAD\nW1AW\n");
        write(
            &f.path("manta.toml"),
            &BASE.replace("[\"k5arh\"]", "[\"k5arh\", \"K5ARH\"]"),
        );
        let report = applied(&f.ctx);
        let lists = f.ctx.lists.take().expect("lists offered");
        assert!(lists.blocklist.contains("W1AW"));
        assert_eq!(lists.blocklist.len(), 2);
        assert_eq!(
            lists.allowlist,
            vec!["k5arh".to_string(), "K5ARH".to_string()]
        );
        assert_eq!(report.blocklist_calls, 2);
        assert_eq!(report.allowlist_calls, 1);
        assert_eq!(report.notch_ranges, 0);
        assert_eq!(report.notch_path, None);
        assert_eq!(report.blocklist_path, Some(f.path("bl.txt")));
        assert!(
            report.restart_needed.is_empty(),
            "{:?}",
            report.restart_needed
        );
    }

    #[test]
    fn missing_blocklist_is_rejected_and_nothing_is_applied() {
        let f = start(
            &format!("{BASE}{}", uplink_block(true, 7000)),
            CliOverrides::none(),
        );
        std::fs::remove_file(f.path("bl.txt")).unwrap();
        write(
            &f.path("manta.toml"),
            &format!("{BASE}{}", uplink_block(false, 7000)),
        );
        let err = format!("{:#}", reload_once(&f.ctx).unwrap_err());
        assert!(err.contains("reading blocklist file"), "{err}");
        assert!(
            err.contains(&f.path("bl.txt").display().to_string()),
            "{err}"
        );
        assert!(f.ctx.lists.take().is_none());
        assert_eq!(flags(&f.ctx), vec![true]);
    }

    /// D7: once the daemon drains, a valid reload applies nothing.
    #[test]
    fn a_reload_after_the_drain_gate_closes_applies_nothing() {
        let f = start(
            &format!("{BASE}{}", uplink_block(true, 7000)),
            CliOverrides::none(),
        );
        write(
            &f.path("manta.toml"),
            &format!("{BASE}{}", uplink_block(false, 7000)),
        );
        f.ctx.drain_gate.close();
        assert!(matches!(
            reload_once(&f.ctx),
            Ok(ReloadOutcome::ShuttingDown)
        ));
        assert!(f.ctx.lists.take().is_none());
        assert_eq!(flags(&f.ctx), vec![true]);
    }

    /// A stop request during list I/O must prevent application even when
    /// the source has not returned and the drain gate is still open.
    #[cfg(unix)]
    #[test]
    fn stop_during_list_read_ignores_the_reload_before_drain() {
        use std::io::Write as _;
        use std::os::unix::ffi::OsStrExt as _;

        let f = start(
            &format!("{BASE}{}", uplink_block(true, 7000)),
            CliOverrides::none(),
        );
        write(
            &f.path("manta.toml"),
            &format!("{BASE}{}", uplink_block(false, 7000)),
        );
        let path = f.path("bl.txt");
        std::fs::remove_file(&path).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is a valid, NUL-terminated path for this test's tempdir.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let result = std::thread::scope(|scope| {
            let reload = scope.spawn(|| reload_once(&f.ctx));
            // Opening the writer completes only once prepare_config has
            // opened the reader. Keep it open until the stop flag is set.
            let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.ctx.stop.store(true, Ordering::Relaxed);
            writer.write_all(b"W1AW\n").unwrap();
            drop(writer);
            reload.join().unwrap()
        });
        assert!(!f.ctx.drain_gate.is_closed());
        assert!(matches!(result, Ok(ReloadOutcome::ShuttingDown)));
        assert!(f.ctx.lists.take().is_none());
        assert_eq!(flags(&f.ctx), vec![true]);
    }

    #[test]
    fn non_utf8_blocklist_is_rejected() {
        let f = start(BASE, CliOverrides::none());
        std::fs::write(f.path("bl.txt"), b"\xff\xfeW\x001\x00").unwrap();
        let err = format!("{:#}", reload_once(&f.ctx).unwrap_err());
        assert!(err.contains("stream did not contain valid UTF-8"), "{err}");
        assert!(f.ctx.lists.take().is_none());
    }

    #[test]
    fn malformed_toml_is_rejected() {
        let f = start(BASE, CliOverrides::none());
        write(&f.path("manta.toml"), "[spot\n");
        let err = format!("{:#}", reload_once(&f.ctx).unwrap_err());
        assert!(err.contains("parsing config file"), "{err}");
        assert!(f.ctx.lists.take().is_none());
    }

    #[test]
    fn command_line_blocklist_keeps_winning() {
        let dir = tempfile::tempdir().unwrap();
        let cli_list = dir.path().join("cli.txt");
        write(&cli_list, "N0CLI\n");
        let mut cli = CliOverrides::none();
        cli.blocklist = Some(cli_list.clone());
        let f = start(BASE, cli);

        write(&f.path("bl.txt"), "W1AW\n");
        let report = applied(&f.ctx);
        let lists = f.ctx.lists.take().unwrap();
        assert!(
            !lists.blocklist.contains("W1AW"),
            "the file's list must not apply"
        );
        assert!(lists.blocklist.contains("N0CLI"));
        assert_eq!(report.blocklist_path, Some(cli_list.clone()));

        write(&cli_list, "N0CLI\nN1CLI\n");
        let report = applied(&f.ctx);
        assert!(f.ctx.lists.take().unwrap().blocklist.contains("N1CLI"));
        assert_eq!(report.blocklist_calls, 2);
    }

    #[test]
    fn dry_run_flip_sets_the_flag_and_reports_the_transition() {
        let f = start(
            &format!(
                "{BASE}{}{}",
                uplink_block(true, 7000),
                uplink_block(true, 7001)
            ),
            CliOverrides::none(),
        );
        write(
            &f.path("manta.toml"),
            &format!(
                "{BASE}{}{}",
                uplink_block(false, 7000),
                uplink_block(true, 7001)
            ),
        );
        let report = applied(&f.ctx);
        assert_eq!(flags(&f.ctx), vec![false, true]);
        assert_eq!(
            report.dry_run_changes,
            vec![("127.0.0.1:7000".to_string(), false)]
        );
        assert!(!report.dry_run_skipped);
        assert!(
            report.restart_needed.is_empty(),
            "{:?}",
            report.restart_needed
        );
    }

    /// A disabled target transmits nothing either way, so its flip is not
    /// logged as one (PR #232 review).
    #[test]
    fn a_disabled_targets_dry_run_flip_is_not_reported() {
        let disabled =
            |dry_run| uplink_block(dry_run, 7000).replace("enabled = true", "enabled = false");
        let f = start(&format!("{BASE}{}", disabled(true)), CliOverrides::none());
        write(&f.path("manta.toml"), &format!("{BASE}{}", disabled(false)));
        let report = applied(&f.ctx);
        assert!(
            report.dry_run_changes.is_empty(),
            "{:?}",
            report.dry_run_changes
        );
        assert!(!report.dry_run_skipped);
    }

    /// What `manta config check` rejects beyond `run`'s config stage, a
    /// reload rejects too, before anything applies (PR #232 review).
    #[test]
    fn config_check_rejections_reject_the_reload() {
        for (from, to, want) in [
            (
                "telnet_port = 0\njson_port = 0",
                "telnet_port = 7300\njson_port = 7300",
                "json_port 7300 is the same as telnet_port",
            ),
            (
                "telnet_port = 0",
                "telnet_port = 0\noperator_qth = \"<your-qth>\"",
                "server.operator_qth is still a placeholder (\"<your-qth>\")",
            ),
        ] {
            let f = start(
                &format!("{BASE}{}", uplink_block(true, 7000)),
                CliOverrides::none(),
            );
            write(&f.path("bl.txt"), "W1AW\n");
            write(
                &f.path("manta.toml"),
                &format!("{}{}", BASE.replace(from, to), uplink_block(false, 7000)),
            );
            let err = format!("{:#}", reload_once(&f.ctx).unwrap_err());
            assert!(err.contains(want), "{err}");
            assert!(f.ctx.lists.take().is_none());
            assert_eq!(flags(&f.ctx), vec![true]);
        }
    }

    /// MAN-268 D3: a command-line source flag replaces `[input]`, so its
    /// placeholders do not reject a reload; without one they do.
    #[test]
    fn input_placeholders_reject_a_reload_only_without_a_source_flag() {
        let config = format!(
            "{BASE}\n[input]\ntype = \"kiwi\"\nhost = \"<your-receiver-host>\"\nfreq_hz = 7e6\n"
        );
        let mut cli = CliOverrides::none();
        cli.kiwi.host = Some("rx.example.org".to_string());
        let f = start(&config, cli);
        assert!(reload_once(&f.ctx).is_ok());
        assert!(f.ctx.lists.take().is_some());

        let f = start(&config, CliOverrides::none());
        let err = format!("{:#}", reload_once(&f.ctx).unwrap_err());
        assert!(err.contains("input.host is still a placeholder"), "{err}");
        assert!(f.ctx.lists.take().is_none());
    }

    #[test]
    fn uplink_structure_change_skips_every_dry_run_change() {
        let f = start(
            &format!("{BASE}{}", uplink_block(true, 7000)),
            CliOverrides::none(),
        );
        write(
            &f.path("manta.toml"),
            &format!("{BASE}{}", uplink_block(false, 7002)),
        );
        let report = applied(&f.ctx);
        assert_eq!(flags(&f.ctx), vec![true]);
        assert!(report.dry_run_skipped);
        assert!(report.dry_run_changes.is_empty());
        assert_eq!(report.restart_needed, vec!["rbn_uplink[0].target_port"]);
        // The lists still apply.
        assert!(f.ctx.lists.take().is_some());
    }

    #[test]
    fn restart_only_changes_reports_cty_and_scp_paths() {
        let old = table("[spot]\ncty_path = \"/a/cty.dat\"\nblocklist_path = \"bl.txt\"\n");
        let new = table("[spot]\ncty_path = \"/b/cty.dat\"\nscp_path = \"/b/master.scp\"\nblocklist_path = \"other.txt\"\n");
        assert_eq!(
            restart_only_changes(&old, &new),
            vec!["spot.cty_path", "spot.scp_path"]
        );
    }

    #[test]
    fn restart_only_changes_reports_a_spot_table_appearing_with_cty_path() {
        let new = table("[spot]\nallowlist = [\"K1A\"]\ncty_path = \"/a/cty.dat\"\n");
        assert_eq!(
            restart_only_changes(&toml::Table::new(), &new),
            vec!["spot.cty_path"]
        );
    }

    #[test]
    fn restart_only_changes_reports_uplink_spot_types() {
        let old = table("[[rbn_uplink]]\ndry_run = true\n");
        let new = table("[[rbn_uplink]]\ndry_run = false\nspot_types = \"all\"\n");
        assert_eq!(
            restart_only_changes(&old, &new),
            vec!["rbn_uplink[0].spot_types"]
        );
    }

    #[test]
    fn restart_only_changes_reports_a_server_key() {
        let old = table("[server]\ntelnet_port = 7300\n");
        let new = table("[server]\ntelnet_port = 7301\n");
        assert_eq!(restart_only_changes(&old, &new), vec!["server.telnet_port"]);
    }

    #[test]
    fn restart_only_changes_ignores_spot_and_uplink_dry_run() {
        let old =
            table("[spot]\nallowlist = [\"A\"]\n[[rbn_uplink]]\ntarget_port = 1\ndry_run = true\n");
        let new = table(
            "[spot]\nallowlist = [\"B\", \"C\"]\nnotch_path = \"n.txt\"\n\
             [[rbn_uplink]]\ntarget_port = 1\ndry_run = false\n",
        );
        assert!(restart_only_changes(&old, &new).is_empty());
    }

    #[test]
    fn restart_only_changes_reports_an_uplink_count_change_whole() {
        let old = table("[[rbn_uplink]]\ntarget_port = 1\n");
        let new = table("[[rbn_uplink]]\ntarget_port = 1\n[[rbn_uplink]]\ntarget_port = 2\n");
        assert_eq!(restart_only_changes(&old, &new), vec!["rbn_uplink"]);
    }

    #[test]
    fn restart_only_changes_ignores_a_spot_table_appearing() {
        let old = table("[server]\ntelnet_port = 1\n");
        let new = table("[server]\ntelnet_port = 1\n[spot]\nallowlist = [\"A\"]\n");
        assert!(restart_only_changes(&old, &new).is_empty());
    }

    #[test]
    fn restart_only_changes_output_is_sorted() {
        let old =
            table("[server]\ntelnet_port = 1\njson_port = 1\n[decode]\nengine = \"legacy\"\n");
        let new = table("[server]\ntelnet_port = 2\njson_port = 2\n[decode]\nengine = \"hsmm\"\n");
        assert_eq!(
            restart_only_changes(&old, &new),
            vec!["decode.engine", "server.json_port", "server.telnet_port"]
        );
    }
}
