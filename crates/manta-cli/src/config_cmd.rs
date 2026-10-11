//! MAN-76: `manta config check` validates a config file the way `manta run`
//! would, without opening the receiver or binding a port, and `manta config
//! init` writes a commented scaffold of every key. See
//! docs/DECISIONS/2026-10-07-man76-config-check-init.md.

use crate::config::{Loaded, SourceFromFile};
use anyhow::{bail, Context, Result};
use manta_server::config::ServerConfig;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Where `init` writes, and the file `check` falls back to (D2, D8).
pub(crate) const DEFAULT_PATH: &str = "manta.toml";

/// The scaffold `init` writes (D9-D11). Pinned to the loader's key set and
/// the code defaults by this module's tests.
pub(crate) const SCAFFOLD: &str = include_str!("config_init.toml");

/// The callsign the scaffold shows where a real one belongs (D10).
pub(crate) const EXAMPLE_CALLSIGN: &str = "N0CALL";

/// Where `check` found the file it checked (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigSource {
    Flag,
    Env,
    /// `./manta.toml`, which `run`/`soak`/`doctor` never read on their own.
    CurrentDir,
    None,
}

/// `manta config check` (D2-D6): `run`'s pre-I/O config pipeline, then a
/// summary on stdout and notes on stderr.
pub(crate) fn check(config_flag: Option<PathBuf>) -> Result<()> {
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let (path, source) = match (config_flag, crate::config::config_path_from_env(&vars)) {
        (Some(p), _) => (Some(p), ConfigSource::Flag),
        (None, Some(p)) => (Some(p), ConfigSource::Env),
        (None, None) if Path::new(DEFAULT_PATH).is_file() => {
            (Some(PathBuf::from(DEFAULT_PATH)), ConfigSource::CurrentDir)
        }
        (None, None) => (None, ConfigSource::None),
    };
    // Exactly `run`'s config stage: load + MANTA_* overlay, resolve (which
    // rejects source types this build cannot open), blocklist/notch reads.
    // Never `spec.open()`, never `is_rf_aware()` (it opens IQ WAVs), never
    // `start_spot_server`.
    let prepared = crate::prepare_live(crate::CliOverrides::none(), path.clone(), None)?;
    reject_placeholders(&prepared.loaded)?;
    reject_duplicate_ports(&prepared.loaded)?;

    let origin_line = match (&path, source) {
        (Some(p), ConfigSource::Flag) => format!("{}: valid (from --config)", p.display()),
        (Some(p), ConfigSource::Env) => format!("{}: valid (from MANTA_CONFIG)", p.display()),
        (Some(p), _) => format!("{}: valid (found in the current directory)", p.display()),
        (None, _) => {
            "no config file: valid (built-in defaults plus any MANTA_* variables)".to_string()
        }
    };
    let mut stdout = std::io::stdout().lock();
    for line in summary(&origin_line, &prepared.loaded) {
        writeln!(stdout, "{line}")?;
    }
    stdout.flush()?;
    for note in notes(&prepared.loaded, &prepared.resolved, source) {
        eprintln!("{note}");
    }
    Ok(())
}

/// `manta config init` (D8): writes `SCAFFOLD` to `out` (`-` is stdout),
/// never replacing an existing file unless `force`.
pub(crate) fn init(out: &Path, force: bool) -> Result<()> {
    if out == Path::new("-") {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(SCAFFOLD.as_bytes())?;
        stdout.flush()?;
        return Ok(());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = match options.open(out) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "{} already exists; pass --force to replace it, or --out to write somewhere else",
            out.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("writing {}", out.display())),
    };
    file.write_all(SCAFFOLD.as_bytes())
        .with_context(|| format!("writing {}", out.display()))?;
    eprintln!(
        "wrote {path} (every setting commented out at its default); edit it, then run \
         `manta config check --config {path}`",
        path = out.display()
    );
    Ok(())
}

/// D3/D10: an uncommented but unedited scaffold example is a definite
/// misconfiguration -- a `<...>` placeholder string anywhere, or the
/// example callsign as the station or uplink login.
fn reject_placeholders(loaded: &Loaded) -> Result<()> {
    if let Some((key, value)) = find_placeholder(&loaded.raw, "") {
        // Never echo a secret, even one shaped like a placeholder (D5).
        if key.ends_with("password") {
            bail!(
                "{}: {key} is still a placeholder -- replace it",
                loaded.origin
            );
        }
        bail!(
            "{}: {key} is still a placeholder ({value:?}) -- replace it",
            loaded.origin
        );
    }
    reject_example_callsigns(loaded)
}

/// MAN-268: the callsign half of `reject_placeholders`, which `run` (and
/// its `listen` alias) also applies, after `prepare_live` and before any
/// source or listener opens, so an unedited manta.example.toml cannot start
/// a daemon that spots, or logs in to a collector, as N0CALL. `loaded` is
/// after the `MANTA_*` overlay, so this checks the identities the daemon
/// would use. Callsigns only: `run` skips the broad `<...>` scan, because a
/// source flag replaces `[input]` and its unused placeholders. See
/// docs/DECISIONS/2026-10-10-man268-unattended-packaging.md.
pub(crate) fn reject_example_callsigns(loaded: &Loaded) -> Result<()> {
    match example_callsign_key(loaded) {
        Some(key) => Err(example_callsign(loaded, key)),
        None => Ok(()),
    }
}

/// The first identity key still holding the example callsign, as `run`
/// checks them. Shared with `manta doctor`'s `config` check (MAN-126), so
/// the two cannot drift.
pub(crate) fn example_callsign_key(loaded: &Loaded) -> Option<&'static str> {
    if let Some(server) = &loaded.server {
        if server
            .station_callsign
            .eq_ignore_ascii_case(EXAMPLE_CALLSIGN)
        {
            return Some("server.station_callsign");
        }
    }
    loaded
        .rbn_uplink
        .iter()
        .any(|uplink| {
            uplink
                .login_callsign
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(EXAMPLE_CALLSIGN))
        })
        .then_some("rbn_uplink.login_callsign")
}

/// D10: `key` still holds the scaffold's example callsign.
fn example_callsign(loaded: &Loaded, key: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: {key} is still the example \"{EXAMPLE_CALLSIGN}\" -- set your own callsign",
        loaded.origin
    )
}

/// The first string value of the form `<...>`, as `(table.key, value)`.
/// Arrays (including `[[rbn_uplink]]`) keep their parent's key.
fn find_placeholder(table: &toml::Table, prefix: &str) -> Option<(String, String)> {
    fn walk(value: &toml::Value, key: &str) -> Option<(String, String)> {
        match value {
            toml::Value::String(s) if s.len() >= 2 && s.starts_with('<') && s.ends_with('>') => {
                Some((key.to_string(), s.clone()))
            }
            toml::Value::Table(t) => find_placeholder(t, key),
            toml::Value::Array(items) => items.iter().find_map(|v| walk(v, key)),
            _ => None,
        }
    }
    table.iter().find_map(|(k, v)| {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        walk(v, &key)
    })
}

/// D3: two servers on one non-zero port and overlapping addresses can
/// never both bind; `run` would fail only after the receiver is already
/// open. MAN-132: telnet and JSON share `bind_addr`, metrics has its own
/// `metrics_bind_addr`.
fn reject_duplicate_ports(loaded: &Loaded) -> Result<()> {
    let Some(server) = &loaded.server else {
        return Ok(());
    };
    let listeners = server_listeners(server);
    for (i, l) in listeners.iter().enumerate() {
        let Some(first) = duplicate_of(&listeners, i) else {
            continue;
        };
        let overlap = if l.addr == first.addr {
            String::new()
        } else {
            format!(
                ", and {} \"{}\" overlaps {} \"{}\"",
                l.addr_key, l.addr, first.addr_key, first.addr
            )
        };
        bail!(
            "{}: [server]: {} {} is the same as {}{overlap}; each server needs its own port",
            loaded.origin,
            l.port_key,
            l.port,
            first.port_key
        );
    }
    Ok(())
}

/// One `[server]` listener: what `run` binds, and the keys that set it.
/// MAN-132: telnet and JSON share `bind_addr`, metrics has its own
/// `metrics_bind_addr`.
pub(crate) struct Listener {
    /// `telnet`, `json` or `metrics`.
    pub name: &'static str,
    pub port_key: &'static str,
    pub addr_key: &'static str,
    pub addr: String,
    pub port: u16,
}

/// `[server]`'s three listeners, in the order `run` binds them.
pub(crate) fn server_listeners(server: &ServerConfig) -> [Listener; 3] {
    let listener = |name, port_key, addr_key, addr: &str, port| Listener {
        name,
        port_key,
        addr_key,
        addr: addr.to_string(),
        port,
    };
    [
        listener(
            "telnet",
            "telnet_port",
            "bind_addr",
            &server.bind_addr,
            server.telnet_port,
        ),
        listener(
            "json",
            "json_port",
            "bind_addr",
            &server.bind_addr,
            server.json_port,
        ),
        listener(
            "metrics",
            "metrics_port",
            "metrics_bind_addr",
            &server.metrics_bind_addr,
            server.metrics_port,
        ),
    ]
}

/// D3: the earlier listener that `listeners[i]` can never bind beside --
/// one non-zero port on overlapping addresses.
pub(crate) fn duplicate_of(listeners: &[Listener], i: usize) -> Option<&Listener> {
    let l = &listeners[i];
    if l.port == 0 {
        return None;
    }
    listeners[..i]
        .iter()
        .find(|first| first.port == l.port && may_overlap(&first.addr, &l.addr))
}

/// Whether two listeners on one port could fail to both bind. Only two
/// different, specific IP literals are provably separate: on Linux a
/// wildcard and a specific address on one port collide (EADDRINUSE),
/// and a host name is not resolved here (D4: no I/O).
fn may_overlap(a: &str, b: &str) -> bool {
    match (a.parse::<std::net::IpAddr>(), b.parse::<std::net::IpAddr>()) {
        (Ok(x), Ok(y)) => x == y || x.is_unspecified() || y.is_unspecified(),
        _ => true,
    }
}

fn engine_name(engine: manta_decode::decoder::Engine) -> &'static str {
    use manta_decode::decoder::Engine;
    match engine {
        Engine::Legacy => "legacy",
        Engine::EdgeLegacy => "edge-legacy",
        Engine::Hsmm => "hsmm",
    }
}

fn line_format_name(format: manta_server::rbn::LineFormat) -> &'static str {
    use manta_server::rbn::LineFormat;
    match format {
        LineFormat::Rbn => "rbn",
        LineFormat::Skimmer => "skimmer",
    }
}

/// The keys of `[table]` that the file or environment actually set, as
/// `key=value` tokens with their written values, minus `skip`.
fn set_keys(loaded: &Loaded, table: &str, skip: &[&str]) -> Vec<String> {
    loaded
        .raw
        .get(table)
        .and_then(toml::Value::as_table)
        .map(|t| {
            t.iter()
                .filter(|(k, _)| !skip.contains(&k.as_str()))
                .map(|(k, v)| format!("{k}={v}"))
                .collect()
        })
        .unwrap_or_default()
}

/// D6: one line per table, `key=value` tokens named as the config keys.
/// Never prints a secret; deterministic (no timestamps, no bound ports).
fn summary(origin_line: &str, loaded: &Loaded) -> Vec<String> {
    let mut lines = vec![origin_line.to_string()];
    lines.push(if loaded.env_vars.is_empty() {
        "environment: none".to_string()
    } else {
        format!("environment: {}", loaded.env_vars.join(", "))
    });

    lines.push(match &loaded.server {
        None => "server: none (manta run starts no servers)".to_string(),
        Some(s) => {
            let mut l = format!(
                "server: station_callsign={} bind_addr={} metrics_bind_addr={} telnet_port={} \
                 json_port={} metrics_port={} line_format={}",
                s.station_callsign,
                s.bind_addr,
                s.metrics_bind_addr,
                s.telnet_port,
                s.json_port,
                s.metrics_port,
                line_format_name(s.line_format)
            );
            let optional = [
                ("status_interval_secs", s.status_interval_secs),
                (
                    "telnet_max_connections_per_ip",
                    s.telnet_max_connections_per_ip.map(|v| v as u64),
                ),
                (
                    "json_max_connections_per_ip",
                    s.json_max_connections_per_ip.map(|v| v as u64),
                ),
                (
                    "metrics_max_connections_per_ip",
                    s.metrics_max_connections_per_ip.map(|v| v as u64),
                ),
                (
                    "telnet_max_commands_per_ip",
                    s.telnet_max_commands_per_ip.map(u64::from),
                ),
                (
                    "json_max_pings_per_ip",
                    s.json_max_pings_per_ip.map(u64::from),
                ),
                (
                    "metrics_max_requests_per_ip",
                    s.metrics_max_requests_per_ip.map(u64::from),
                ),
            ];
            for (key, value) in optional {
                if let Some(v) = value {
                    l.push_str(&format!(" {key}={v}"));
                }
            }
            // MAN-86: the greeting-banner identity. Free text is quoted so a
            // QTH with spaces stays one token; the grid is validated as a
            // bare locator.
            if let Some(v) = &s.operator_name {
                l.push_str(&format!(" operator_name={v:?}"));
            }
            if let Some(v) = &s.operator_qth {
                l.push_str(&format!(" operator_qth={v:?}"));
            }
            if let Some(v) = &s.operator_grid {
                l.push_str(&format!(" operator_grid={v}"));
            }
            l
        }
    });

    if loaded.rbn_uplink.is_empty() {
        lines.push("rbn_uplink: none".to_string());
    }
    let station = loaded
        .server
        .as_ref()
        .map(|s| s.station_callsign.as_str())
        .unwrap_or_default();
    for u in &loaded.rbn_uplink {
        lines.push(format!(
            "rbn_uplink: target_host={} target_port={} enabled={} dry_run={} spot_types={} \
             login_callsign={}",
            u.target_host,
            u.target_port,
            u.enabled,
            u.dry_run,
            u.spot_types.as_str(),
            u.effective_login_callsign(station)
        ));
    }

    let mut input = match &loaded.input.source {
        None => "input: type=unset (the default sound card, unless a command-line flag picks \
                 the source)"
            .to_string(),
        Some(SourceFromFile::Audio { device }) => match device {
            Some(d) => format!("input: type=audio device={d:?}"),
            None => "input: type=audio device=default".to_string(),
        },
        Some(SourceFromFile::File { path, iq }) => {
            format!("input: type=file path={} iq={iq}", path.display())
        }
        Some(SourceFromFile::Kiwi {
            host,
            port,
            freq_hz,
            password,
        }) => format!(
            "input: type=kiwi host={host} port={port} freq_hz={freq_hz} password={}",
            if password.is_empty() { "none" } else { "set" }
        ),
        Some(SourceFromFile::Soapy {
            driver,
            freq_hz,
            rate_hz,
            gain_db,
        }) => format!(
            "input: type=soapy driver={driver:?} freq_hz={freq_hz} rate_hz={rate_hz} gain_db={}",
            gain_db.map_or("auto".to_string(), |g| g.to_string())
        ),
        Some(SourceFromFile::Hpsdr {
            host,
            port,
            freq_hz,
            rate_hz,
        }) => {
            format!("input: type=hpsdr host={host} port={port} freq_hz={freq_hz} rate_hz={rate_hz}")
        }
    };
    let shared = &loaded.input.shared;
    for (key, value) in [
        (
            "freq_correction_ppm",
            shared.freq_correction_ppm.map(|v| v.to_string()),
        ),
        (
            "center_freq_hz",
            shared.center_freq_hz.map(|v| v.to_string()),
        ),
        (
            "capture_rate_hz",
            shared.capture_rate_hz.map(|v| v.to_string()),
        ),
        ("replay_epoch", shared.replay_epoch.map(|v| v.to_string())),
    ] {
        if let Some(v) = value {
            input.push_str(&format!(" {key}={v}"));
        }
    }
    lines.push(input);

    let path_or_none = |p: &Option<PathBuf>| {
        p.as_ref()
            .map_or("none".to_string(), |p| p.display().to_string())
    };
    lines.push(format!(
        "spot: allowlist={} blocklist_path={} notch_path={} cty_path={} scp_path={}",
        if loaded.spot.allowlist.is_empty() {
            "none".to_string()
        } else {
            loaded.spot.allowlist.join(",")
        },
        path_or_none(&loaded.spot.blocklist_path),
        path_or_none(&loaded.spot.notch_path),
        loaded
            .spot
            .cty_path
            .as_ref()
            .map_or("bundled".to_string(), |p| p.display().to_string()),
        loaded
            .spot
            .scp_path
            .as_ref()
            .map_or("bundled".to_string(), |p| p.display().to_string())
    ));

    let detector = set_keys(loaded, "detector", &[]);
    lines.push(if detector.is_empty() {
        "detector: defaults".to_string()
    } else {
        format!("detector: {} (other keys default)", detector.join(" "))
    });
    let mut decode = vec![format!("engine={}", engine_name(loaded.decode.engine))];
    decode.extend(set_keys(loaded, "decode", &["engine"]));
    lines.push(format!("decode: {} (other keys default)", decode.join(" ")));
    lines
}

/// D3/D4: stderr notes that never change the exit code. I/O-free: the dial
/// note matches the source kind rather than calling `is_rf_aware()`, which
/// opens IQ WAVs.
fn notes(loaded: &Loaded, resolved: &crate::Resolved, source: ConfigSource) -> Vec<String> {
    let mut notes = Vec::new();
    match source {
        ConfigSource::None => notes.push(format!(
            "note: no config file given (--config, MANTA_CONFIG) or found ({DEFAULT_PATH} in \
             the current directory); checked the built-in defaults plus any MANTA_* variables"
        )),
        ConfigSource::CurrentDir => notes.push(format!(
            "note: manta run does not read {DEFAULT_PATH} from the current directory on its \
             own -- run it with --config {DEFAULT_PATH} or MANTA_CONFIG={DEFAULT_PATH}"
        )),
        ConfigSource::Flag | ConfigSource::Env => {}
    }
    let Some(server) = &loaded.server else {
        return notes;
    };
    if resolved.dial_freq_hz.is_none()
        && matches!(
            resolved.spec,
            crate::LiveSourceSpec::AudioDevice(_) | crate::LiveSourceSpec::File { .. }
        )
    {
        notes.push(
            "note: manta run with this config needs your radio's dial frequency -- set \
             input.center_freq_hz, MANTA_INPUT_CENTER_FREQ_HZ or --dial-freq-hz (an IQ \
             recording whose sidecar names its centre frequency does not need it)"
                .to_string(),
        );
    }
    bind_notes(&mut notes, "bind_addr", &server.bind_addr, |addr| {
        format!(
            "note: the telnet and JSON servers listen on every network interface (bind_addr = \
             \"{addr}\"), as a public cluster node does -- set bind_addr = \"127.0.0.1\" to keep \
             them local"
        )
    });
    bind_notes(
        &mut notes,
        "metrics_bind_addr",
        &server.metrics_bind_addr,
        |addr| {
            format!(
                "note: the metrics endpoint listens on every network interface \
                 (metrics_bind_addr = \"{addr}\") and has no password -- firewall metrics_port \
                 to the machines that scrape it, or set metrics_bind_addr = \"127.0.0.1\""
            )
        },
    );
    notes
}

/// D3 (MAN-76), split per listener by MAN-132: `bind_addr` serves
/// telnet/JSON, `metrics_bind_addr` the password-less metrics endpoint.
fn bind_notes(notes: &mut Vec<String>, key: &str, addr: &str, public: impl Fn(&str) -> String) {
    match addr.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_unspecified() => notes.push(public(addr)),
        Ok(_) => {}
        Err(_) if addr == "localhost" => {}
        Err(_) => notes.push(format!(
            "note: {key} = \"{addr}\" is not an IP address; manta run looks it up as a host \
             name when it binds, and fails then if it does not resolve"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Env, InputKind};
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    fn load_text(body: &str) -> Result<Loaded> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, body).unwrap();
        config::load(Some(&path), Env::Ignore)
    }

    fn loaded(body: &str) -> Loaded {
        load_text(body).unwrap_or_else(|e| panic!("{body:?} did not load: {e:#}"))
    }

    fn resolved(loaded: &Loaded) -> crate::Resolved {
        crate::resolve(crate::CliOverrides::none(), loaded).unwrap()
    }

    fn lines_of(body: &str) -> Vec<String> {
        summary("manta.toml: valid (from --config)", &loaded(body))
    }

    fn line(lines: &[String], prefix: &str) -> String {
        lines
            .iter()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no `{prefix}` line in {lines:#?}"))
            .clone()
    }

    // ---- Phase 2: the summary and notes

    #[test]
    fn summary_server_line_shows_configured_ports_and_defaults() {
        let lines = lines_of("[server]\nstation_callsign = \"w1aw\"\n");
        assert_eq!(
            line(&lines, "server:"),
            "server: station_callsign=W1AW bind_addr=0.0.0.0 metrics_bind_addr=127.0.0.1 \
             telnet_port=7300 json_port=7301 metrics_port=7302 line_format=rbn"
        );
        let lines =
            lines_of("[server]\nstation_callsign = \"W1AW\"\nmetrics_bind_addr = \"0.0.0.0\"\n");
        let server = line(&lines, "server:");
        assert!(server.contains(" metrics_bind_addr=0.0.0.0 "), "{server}");
        let lines = lines_of(
            "[server]\nstation_callsign = \"W1AW\"\ntelnet_port = 0\nstatus_interval_secs = 0\n\
             line_format = \"skimmer\"\n",
        );
        let server = line(&lines, "server:");
        assert!(server.contains(" telnet_port=0 "), "{server}");
        assert!(server.contains(" line_format=skimmer"), "{server}");
        assert!(server.ends_with(" status_interval_secs=0"), "{server}");
    }

    #[test]
    fn summary_server_line_shows_operator_identity_only_when_set() {
        // MAN-86 (PR #128 review): the Aggregator-facing identity keys are
        // `Option`s, so D6 lists them when set and omits them when absent.
        // Free text is quoted so a QTH with spaces stays one token.
        let lines = lines_of("[server]\nstation_callsign = \"W1AW\"\n");
        assert!(!line(&lines, "server:").contains("operator_"));
        let lines = lines_of(
            "[server]\nstation_callsign = \"HB9H\"\noperator_name = \"Art\"\n\
             operator_qth = \"Richmond Hill, ON\"\noperator_grid = \"FN03gw\"\n",
        );
        let server = line(&lines, "server:");
        assert!(
            server.ends_with(
                " operator_name=\"Art\" operator_qth=\"Richmond Hill, ON\" operator_grid=FN03gw"
            ),
            "{server}"
        );
    }

    #[test]
    fn summary_without_server_says_no_servers() {
        let lines = lines_of("[decode]\nengine = \"legacy\"\n");
        assert_eq!(
            line(&lines, "server:"),
            "server: none (manta run starts no servers)"
        );
        assert_eq!(line(&lines, "rbn_uplink:"), "rbn_uplink: none");
    }

    #[test]
    fn summary_lists_each_rbn_uplink() {
        let lines = lines_of(
            "[server]\nstation_callsign = \"W1AW\"\n\
             [[rbn_uplink]]\nenabled = true\ntarget_host = \"a.example.org\"\ntarget_port = 7000\n\
             [[rbn_uplink]]\nenabled = false\ntarget_host = \"b.example.org\"\ntarget_port = 7001\n\
             login_callsign = \"k1abc\"\ndry_run = false\nspot_types = \"all\"\n",
        );
        let uplinks: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with("rbn_uplink:"))
            .collect();
        assert_eq!(
            uplinks,
            [
                "rbn_uplink: target_host=a.example.org target_port=7000 enabled=true \
                 dry_run=true spot_types=cq_beacon login_callsign=W1AW",
                "rbn_uplink: target_host=b.example.org target_port=7001 enabled=false \
                 dry_run=false spot_types=all login_callsign=K1ABC",
            ]
        );
    }

    #[test]
    fn summary_input_line_per_type() {
        let lines = lines_of("[input]\nfreq_correction_ppm = 2.5\n");
        assert_eq!(
            line(&lines, "input:"),
            "input: type=unset (the default sound card, unless a command-line flag picks the \
             source) freq_correction_ppm=2.5"
        );
        let lines = lines_of("[input]\ntype = \"audio\"\ndevice = \"USB Audio\"\n");
        assert_eq!(
            line(&lines, "input:"),
            "input: type=audio device=\"USB Audio\""
        );
        let lines = lines_of("[input]\ntype = \"audio\"\n");
        assert_eq!(line(&lines, "input:"), "input: type=audio device=default");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(
            &path,
            "[input]\ntype = \"file\"\npath = \"rec.wav\"\niq = true\ncenter_freq_hz = 14000000.0\n",
        )
        .unwrap();
        let l = config::load(Some(&path), Env::Ignore).unwrap();
        assert_eq!(
            line(&summary("x", &l), "input:"),
            format!(
                "input: type=file path={} iq=true center_freq_hz=14000000",
                dir.path().join("rec.wav").display()
            )
        );

        let lines =
            lines_of("[input]\ntype = \"kiwi\"\nhost = \"rx.example.org\"\nfreq_hz = 7030000.0\n");
        assert_eq!(
            line(&lines, "input:"),
            "input: type=kiwi host=rx.example.org port=8073 freq_hz=7030000 password=none"
        );
        // soapy/hpsdr are rendered from the loaded file, so these hold on
        // every feature set.
        let lines = lines_of(
            "[input]\ntype = \"soapy\"\ndriver = \"driver=rtlsdr\"\nfreq_hz = 7e6\n\
             rate_hz = 192000.0\n",
        );
        assert_eq!(
            line(&lines, "input:"),
            "input: type=soapy driver=\"driver=rtlsdr\" freq_hz=7000000 rate_hz=192000 \
             gain_db=auto"
        );
        let lines = lines_of(
            "[input]\ntype = \"hpsdr\"\nhost = \"10.0.0.5\"\nfreq_hz = 7e6\nrate_hz = 192000.0\n\
             capture_rate_hz = 48000.0\nreplay_epoch = 0\n",
        );
        assert_eq!(
            line(&lines, "input:"),
            "input: type=hpsdr host=10.0.0.5 port=1024 freq_hz=7000000 rate_hz=192000 \
             capture_rate_hz=48000 replay_epoch=0"
        );
    }

    #[test]
    fn summary_never_prints_the_kiwi_password() {
        let lines = lines_of(
            "[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\npassword = \"hunter2secret\"\n",
        );
        assert!(
            line(&lines, "input:").ends_with(" password=set"),
            "{lines:?}"
        );
        for l in &lines {
            assert!(!l.contains("hunter2secret"), "{l}");
        }
    }

    #[test]
    fn summary_spot_line_lists_allowlist_and_resolved_paths() {
        let lines = lines_of("");
        assert_eq!(
            line(&lines, "spot:"),
            "spot: allowlist=none blocklist_path=none notch_path=none cty_path=bundled scp_path=bundled"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        // Absolute on every OS: "/etc/manta/notch.txt" has no drive prefix,
        // so Windows treats it as relative and joins it onto the config dir.
        let notch = std::env::temp_dir().join("etc").join("notch.txt");
        std::fs::write(
            &path,
            format!(
                "[spot]\nallowlist = [\"W1AW\", \"K1ABC\"]\nblocklist_path = \"bad.txt\"\n\
                 notch_path = '{}'\ncty_path = 'cty.dat'\nscp_path = 'MASTER.SCP'\n",
                notch.display()
            ),
        )
        .unwrap();
        let l = config::load(Some(&path), Env::Ignore).unwrap();
        assert_eq!(
            line(&summary("x", &l), "spot:"),
            format!(
                "spot: allowlist=W1AW,K1ABC blocklist_path={} notch_path={} cty_path={} scp_path={}",
                dir.path().join("bad.txt").display(),
                notch.display(),
                dir.path().join("cty.dat").display(),
                dir.path().join("MASTER.SCP").display()
            )
        );
    }

    #[test]
    fn summary_detector_and_decode_show_only_keys_that_were_set() {
        let lines = lines_of("");
        assert_eq!(line(&lines, "detector:"), "detector: defaults");
        assert_eq!(
            line(&lines, "decode:"),
            "decode: engine=legacy (other keys default)"
        );
        let lines = lines_of("[detector]\non_snr_db = 15\n");
        assert_eq!(
            line(&lines, "detector:"),
            "detector: on_snr_db=15 (other keys default)"
        );
        let lines = lines_of("[decode]\nengine = \"hsmm\"\nbeam = 16\n");
        assert_eq!(
            line(&lines, "decode:"),
            "decode: engine=hsmm beam=16 (other keys default)"
        );
    }

    #[test]
    fn summary_environment_line_names_applied_manta_vars() {
        let vars: Vec<(OsString, OsString)> = [
            ("MANTA_SERVER_TELNET_PORT", "9300"),
            ("MANTA_CONFIG", "/etc/manta.toml"),
            ("MANTA_GIT_SHA", "abc123"),
        ]
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, "[server]\nstation_callsign = \"W1AW\"\n").unwrap();
        let l = config::load(Some(&path), Env::Read(&vars)).unwrap();
        let lines = summary("x", &l);
        assert_eq!(
            line(&lines, "environment:"),
            "environment: MANTA_SERVER_TELNET_PORT"
        );
        assert!(line(&lines, "server:").contains(" telnet_port=9300 "));
        assert_eq!(line(&lines_of(""), "environment:"), "environment: none");
    }

    fn notes_of(body: &str) -> Vec<String> {
        let l = loaded(body);
        let r = resolved(&l);
        notes(&l, &r, ConfigSource::Flag)
    }

    const DIAL_NOTE: &str = "note: manta run with this config needs your radio's dial frequency";
    const PUBLIC_NOTE: &str = "note: the telnet and JSON servers listen on every network interface";
    const METRICS_NOTE: &str = "note: the metrics endpoint listens on every network interface";

    fn has(notes: &[String], prefix: &str) -> bool {
        notes.iter().any(|n| n.starts_with(prefix))
    }

    #[test]
    fn note_dial_frequency_when_server_and_audio_or_file_source_lack_one() {
        let server = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n";
        assert!(has(&notes_of(server), DIAL_NOTE));
        assert!(has(
            &notes_of(&format!(
                "{server}[input]\ntype = \"file\"\npath = \"x.wav\"\niq = true\n"
            )),
            DIAL_NOTE
        ));
        assert!(!has(
            &notes_of(&format!("{server}[input]\ncenter_freq_hz = 14030000.0\n")),
            DIAL_NOTE
        ));
        assert!(!has(
            &notes_of(&format!(
                "{server}[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\n"
            )),
            DIAL_NOTE
        ));
        // No [server]: `run` starts no servers, so it needs no dial.
        assert!(!has(&notes_of(""), DIAL_NOTE));
    }

    /// MAN-132: `bind_addr` serves telnet/JSON and `metrics_bind_addr` the
    /// metrics endpoint, so each gets its own exposure note.
    #[test]
    fn bind_notes_are_per_listener() {
        for addr in ["0.0.0.0", "::"] {
            let n = notes_of(&format!(
                "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"{addr}\"\n"
            ));
            assert!(has(&n, PUBLIC_NOTE), "{addr}: {n:?}");
            assert!(!has(&n, METRICS_NOTE), "{addr}: {n:?}");
        }
        // Scenario 1's default: telnet/JSON public, metrics on loopback.
        let n = notes_of("[server]\nstation_callsign = \"W1AW\"\n");
        assert!(has(&n, PUBLIC_NOTE), "the default is 0.0.0.0: {n:?}");
        assert!(
            !has(&n, METRICS_NOTE),
            "metrics defaults to 127.0.0.1: {n:?}"
        );
        let n = notes_of("[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n");
        assert!(!has(&n, PUBLIC_NOTE) && !has(&n, METRICS_NOTE), "{n:?}");
        for addr in ["0.0.0.0", "::"] {
            let n = notes_of(&format!(
                "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
                 metrics_bind_addr = \"{addr}\"\n"
            ));
            let note = n
                .iter()
                .find(|x| x.starts_with(METRICS_NOTE))
                .unwrap_or_else(|| panic!("{addr}: no metrics note in {n:?}"));
            assert!(
                note.contains("no password") && note.contains("firewall metrics_port"),
                "{note}"
            );
            assert!(!has(&n, PUBLIC_NOTE), "{addr}: {n:?}");
        }
        let n = notes_of(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"0.0.0.0\"\n\
             metrics_bind_addr = \"0.0.0.0\"\n",
        );
        assert!(has(&n, PUBLIC_NOTE) && has(&n, METRICS_NOTE), "{n:?}");
        assert!(
            !n.iter().any(|x| x.contains("telnet, JSON and metrics")),
            "{n:?}"
        );
    }

    #[test]
    fn note_bind_addresses_that_are_not_ip_literals() {
        let n = notes_of("[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"0.0.0.0.0\"\n");
        assert!(
            has(&n, "note: bind_addr = \"0.0.0.0.0\" is not an IP address"),
            "{n:?}"
        );
        let n = notes_of("[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"localhost\"\n");
        assert!(!n.iter().any(|x| x.contains("bind_addr")), "{n:?}");
        let n = notes_of(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_bind_addr = \"prom.lan\"\n",
        );
        assert!(
            has(
                &n,
                "note: metrics_bind_addr = \"prom.lan\" is not an IP address"
            ),
            "{n:?}"
        );
        let n = notes_of(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_bind_addr = \"localhost\"\n",
        );
        assert!(!n.iter().any(|x| x.contains("metrics_bind_addr")), "{n:?}");
    }

    #[test]
    fn note_when_no_config_file_was_found() {
        let l = config::load(None, Env::Ignore).unwrap();
        let r = resolved(&l);
        assert!(has(
            &notes(&l, &r, ConfigSource::None),
            "note: no config file given"
        ));
        for source in [ConfigSource::Flag, ConfigSource::Env] {
            assert!(notes(&l, &r, source).is_empty(), "{source:?}");
        }
    }

    /// `run` reads only --config or MANTA_CONFIG, so a file `check` found
    /// in the current directory must not read as "what run will use".
    #[test]
    fn note_run_needs_config_for_a_file_found_in_the_current_directory() {
        let l = config::load(None, Env::Ignore).unwrap();
        let r = resolved(&l);
        assert_eq!(
            notes(&l, &r, ConfigSource::CurrentDir),
            [
                "note: manta run does not read manta.toml from the current directory on its own \
              -- run it with --config manta.toml or MANTA_CONFIG=manta.toml"
            ]
        );
    }

    #[test]
    fn duplicate_non_zero_server_ports_are_rejected() {
        let l = loaded(
            "[server]\nstation_callsign = \"W1AW\"\ntelnet_port = 17300\njson_port = 17300\n",
        );
        let err = format!("{:#}", reject_duplicate_ports(&l).unwrap_err());
        assert!(
            err.contains("[server]: json_port 17300 is the same as telnet_port"),
            "{err}"
        );
        let l = loaded("[server]\nstation_callsign = \"W1AW\"\nmetrics_port = 7301\n");
        let err = format!("{:#}", reject_duplicate_ports(&l).unwrap_err());
        assert!(
            err.contains("metrics_port 7301 is the same as json_port"),
            "{err}"
        );
        let l = loaded(
            "[server]\nstation_callsign = \"W1AW\"\ntelnet_port = 0\njson_port = 0\nmetrics_port = 0\n",
        );
        assert!(reject_duplicate_ports(&l).is_ok());

        // MAN-132: metrics has its own address, so a shared port is a
        // collision only when the two addresses can overlap.
        let err_of =
            |body: &str| format!("{:#}", reject_duplicate_ports(&loaded(body)).unwrap_err());
        let err = err_of("[server]\nstation_callsign = \"W1AW\"\nmetrics_port = 7300\n");
        assert!(
            err.contains(
                "metrics_port 7300 is the same as telnet_port, and metrics_bind_addr \
                 \"127.0.0.1\" overlaps bind_addr \"0.0.0.0\"; each server needs its own port"
            ),
            "{err}"
        );
        let err = err_of(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\nmetrics_port = 7300\n",
        );
        assert!(
            err.contains(
                "[server]: metrics_port 7300 is the same as telnet_port; each server needs its own port"
            ),
            "{err}"
        );
        let l = loaded(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"192.168.1.5\"\nmetrics_port = 7300\n",
        );
        assert!(
            reject_duplicate_ports(&l).is_ok(),
            "two distinct specific IPs both bind"
        );
        // No DNS lookup in `check` (D4), so a host name may overlap.
        let err = err_of(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"localhost\"\nmetrics_port = 7300\n",
        );
        assert!(err.contains("overlaps"), "{err}");
    }

    // ---- Phase 3: the scaffold and its drift guards

    /// One `#key = value` setting line: `(key, value_src, line_no)`, with
    /// `line_no` 0-based.
    type Setting = (String, String, usize);

    /// The scaffold's table sections: header line -> its setting lines.
    /// A section runs from `[x]`, `#[x]` or `#[[x]]` to the next header.
    fn sections() -> Vec<(String, Vec<Setting>)> {
        let mut out: Vec<(String, Vec<Setting>)> = Vec::new();
        let header = regex::Regex::new(r"^#?\[\[?([a-z_]+)\]\]?\s*$").unwrap();
        let setting = regex::Regex::new(r"^#([a-z_]+) = (.*)$").unwrap();
        for (n, l) in SCAFFOLD.lines().enumerate() {
            if let Some(c) = header.captures(l) {
                out.push((c[1].to_string(), Vec::new()));
            } else if let Some(c) = setting.captures(l) {
                let (_, lines) = out
                    .last_mut()
                    .unwrap_or_else(|| panic!("setting line {} before any table", n + 1));
                lines.push((c[1].to_string(), c[2].to_string(), n));
            }
        }
        out
    }

    /// Every setting line of `table`.
    fn setting_lines(table: &str) -> Vec<Setting> {
        let all: Vec<_> = sections().into_iter().filter(|(t, _)| t == table).collect();
        assert_eq!(
            all.len(),
            1,
            "[{table}] must appear exactly once in the scaffold"
        );
        all.into_iter().next().unwrap().1
    }

    fn scaffold_line(n: usize) -> &'static str {
        SCAFFOLD.lines().nth(n).unwrap()
    }

    /// A setting line with its leading `#` removed; TOML reads the
    /// trailing `# ...` as a comment.
    fn uncomment(line: &str) -> String {
        line.strip_prefix('#').unwrap().to_string()
    }

    fn scaffold_setting(table: &str, key: &str) -> String {
        let (_, _, n) = setting_lines(table)
            .into_iter()
            .find(|(k, _, _)| k == key)
            .unwrap_or_else(|| panic!("the scaffold has no {table}.{key}"));
        uncomment(scaffold_line(n))
    }

    /// Keys with no built-in default: the scaffold shows an example (D10).
    const NO_DEFAULT: &[(&str, &str)] = &[
        ("server", "station_callsign"),
        ("server", "operator_name"),
        ("server", "operator_qth"),
        ("server", "operator_grid"),
        ("rbn_uplink", "enabled"),
        ("rbn_uplink", "target_host"),
        ("rbn_uplink", "target_port"),
        ("rbn_uplink", "login_callsign"),
        ("input", "type"),
        ("input", "device"),
        ("input", "path"),
        ("input", "host"),
        ("input", "freq_hz"),
        ("input", "driver"),
        ("input", "rate_hz"),
        ("input", "gain_db"),
        ("input", "center_freq_hz"),
        ("input", "capture_rate_hz"),
        ("input", "replay_epoch"),
        ("spot", "blocklist_path"),
        ("spot", "notch_path"),
        ("spot", "cty_path"),
        ("spot", "scp_path"),
    ];

    /// `Option` keys whose absence means a built-in value: setting them to
    /// that value changes the typed config (None -> Some) but not behaviour.
    const BUILTIN_EQUIVALENT: &[(&str, &str)] = &[
        ("server", "status_interval_secs"),
        ("server", "telnet_max_connections_per_ip"),
        ("server", "json_max_connections_per_ip"),
        ("server", "metrics_max_connections_per_ip"),
        ("server", "telnet_max_commands_per_ip"),
        ("server", "json_max_pings_per_ip"),
        ("server", "metrics_max_requests_per_ip"),
        ("input", "freq_correction_ppm"),
    ];

    #[test]
    fn scaffold_as_written_loads_as_the_built_in_defaults() {
        let l = loaded(SCAFFOLD);
        assert!(l.server.is_none());
        assert!(l.rbn_uplink.is_empty());
        assert_eq!(
            format!("{:?}", l.decode),
            format!("{:?}", manta_decode::decoder::DecodeConfig::default())
        );
        assert_eq!(l.detector, manta_engine::DetectorConfig::default());
        assert_eq!(l.spot, config::SpotFile::default());
        assert_eq!(l.input, config::InputFile::default());
        assert!(reject_placeholders(&l).is_ok());
    }

    /// The loader's own key list for `table`: serde's `expected one of ...`
    /// for an unknown key (every table is `deny_unknown_fields`).
    fn loader_keys(table: &str) -> BTreeSet<String> {
        let header = if table == "rbn_uplink" {
            "[[rbn_uplink]]".to_string()
        } else {
            format!("[{table}]")
        };
        let err = format!(
            "{:#}",
            load_text(&format!("{header}\n__probe__ = 1\n"))
                .err()
                .unwrap_or_else(|| panic!("[{table}] accepted an unknown key"))
        );
        let list = err
            .split_once("expected one of")
            .unwrap_or_else(|| panic!("[{table}]: no key list in {err:?}"))
            .1;
        let keys: BTreeSet<String> = regex::Regex::new(r"`([a-z_]+)`")
            .unwrap()
            .captures_iter(list)
            .map(|c| c[1].to_string())
            .collect();
        assert!(keys.len() >= 3, "[{table}]: {err}");
        keys
    }

    #[test]
    fn scaffold_lists_exactly_the_keys_the_loader_accepts() {
        let names: Vec<String> = sections().into_iter().map(|(t, _)| t).collect();
        assert_eq!(names, config::KNOWN_TABLES, "scaffold tables, in order");
        for table in config::KNOWN_TABLES {
            let lines = setting_lines(table);
            let listed: Vec<&str> = lines.iter().map(|(k, _, _)| k.as_str()).collect();
            let unique: BTreeSet<String> = listed.iter().map(|k| k.to_string()).collect();
            assert_eq!(
                unique.len(),
                listed.len(),
                "[{table}] lists a key twice: {listed:?}"
            );
            let accepted = loader_keys(table);
            let missing: Vec<_> = accepted.difference(&unique).collect();
            let extra: Vec<_> = unique.difference(&accepted).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "[{table}]: the scaffold lacks {missing:?} and lists unknown {extra:?}; \
                 update src/config_init.toml"
            );
        }
    }

    /// Required context so a single key line loads on its own.
    fn base(table: &str, key: &str) -> String {
        const SERVER: &str = "[server]\nstation_callsign = \"W1AW\"\n";
        match (table, key) {
            ("server", _) => SERVER.to_string(),
            ("rbn_uplink", _) => format!(
                "{SERVER}[[rbn_uplink]]\nenabled = false\ntarget_host = \"h\"\ntarget_port = 1\n"
            ),
            ("input", "port" | "password") => {
                "[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7030000.0\n".to_string()
            }
            // Absolute, so both loads resolve it the same from their temp dirs.
            ("input", "iq") => "[input]\ntype = \"file\"\npath = \"/x.wav\"\n".to_string(),
            _ => format!("[{table}]\n"),
        }
    }

    /// Whether `table` types identically in `a` and `b`: `==`, except
    /// `DecodeConfig`, which has no `PartialEq`, by its `Debug` text.
    fn same_table(a: &Loaded, b: &Loaded, table: &str) -> bool {
        match table {
            "server" => a.server == b.server,
            "rbn_uplink" => a.rbn_uplink == b.rbn_uplink,
            "input" => a.input == b.input,
            "spot" => a.spot == b.spot,
            "detector" => a.detector == b.detector,
            "decode" => format!("{:?}", a.decode) == format!("{:?}", b.decode),
            other => panic!("unknown table {other}"),
        }
    }

    #[test]
    fn every_default_valued_line_is_the_built_in_default() {
        let mut checked = 0;
        for table in config::KNOWN_TABLES {
            for (key, _, n) in setting_lines(table) {
                let id = (*table, key.as_str());
                if NO_DEFAULT.contains(&id) || BUILTIN_EQUIVALENT.contains(&id) {
                    continue;
                }
                let base = base(table, &key);
                let with = format!("{base}{}\n", uncomment(scaffold_line(n)));
                let (a, b) = (loaded(&base), loaded(&with));
                assert!(
                    same_table(&a, &b, table),
                    "{table}.{key}: the scaffold line {:?} is not the built-in default",
                    scaffold_line(n)
                );
                // The line really reached its table (`[[rbn_uplink]]`: the
                // base's one entry).
                let applied = match &b.raw[*table] {
                    toml::Value::Array(items) => items
                        .last()
                        .and_then(toml::Value::as_table)
                        .is_some_and(|t| t.contains_key(&key)),
                    v => v.as_table().is_some_and(|t| t.contains_key(&key)),
                };
                assert!(applied, "{table}.{key}: the line did not reach [{table}]");
                checked += 1;
            }
        }
        assert!(checked >= 40, "only {checked} default-valued lines checked");
    }

    fn scaffold_number(table: &str, key: &str) -> toml::Value {
        let doc: toml::Table = toml::from_str(&scaffold_setting(table, key)).unwrap();
        doc[key].clone()
    }

    #[test]
    fn builtin_equivalent_lines_match_the_code_constants() {
        use manta_server::{json_stream, metrics_http, status, telnet};
        let int = |key: &str| scaffold_number("server", key).as_integer().unwrap() as u64;
        assert_eq!(
            int("telnet_max_connections_per_ip"),
            telnet::MAX_TELNET_CONNECTIONS_PER_IP as u64
        );
        assert_eq!(
            int("json_max_connections_per_ip"),
            json_stream::MAX_JSON_STREAM_CONNECTIONS_PER_IP as u64
        );
        assert_eq!(
            int("metrics_max_connections_per_ip"),
            metrics_http::MAX_METRICS_CONNECTIONS_PER_IP as u64
        );
        assert_eq!(
            int("telnet_max_commands_per_ip"),
            u64::from(telnet::MAX_TELNET_COMMANDS)
        );
        assert_eq!(
            int("json_max_pings_per_ip"),
            u64::from(json_stream::MAX_INBOUND_PINGS)
        );
        assert_eq!(
            int("metrics_max_requests_per_ip"),
            u64::from(metrics_http::MAX_METRICS_REQUESTS_PER_IP)
        );
        assert_eq!(
            int("status_interval_secs"),
            status::DEFAULT_STATUS_INTERVAL.as_secs()
        );
        // `resolve` falls back to 0.0 when no ppm is set.
        let ppm = scaffold_number("input", "freq_correction_ppm");
        assert_eq!(ppm.as_float(), Some(0.0));
        let l = loaded("");
        assert_eq!(resolved(&l).freq_correction_ppm, 0.0);
        // The rate windows the comments quote.
        assert!(
            scaffold_setting("server", "telnet_max_commands_per_ip").contains(&format!(
                "per {} seconds",
                telnet::COMMAND_RATE_WINDOW.as_secs()
            ))
        );
        assert!(
            scaffold_setting("server", "json_max_pings_per_ip").contains(&format!(
                "per {} seconds",
                json_stream::PING_RATE_WINDOW.as_secs()
            ))
        );
        assert!(
            scaffold_setting("server", "metrics_max_requests_per_ip").contains(&format!(
                "per {} seconds",
                metrics_http::METRICS_REQUEST_RATE_WINDOW.as_secs()
            ))
        );
    }

    fn uncommented(table: &str, keep: impl Fn(&str) -> bool) -> String {
        setting_lines(table)
            .into_iter()
            .filter(|(k, _, _)| keep(k))
            .map(|(_, _, n)| uncomment(scaffold_line(n)) + "\n")
            .collect()
    }

    #[test]
    fn no_default_examples_are_valid_when_uncommented() {
        let server = format!("[server]\n{}", uncommented("server", |_| true));
        let l = loaded(&server);
        assert_eq!(
            l.server.as_ref().unwrap().station_callsign,
            EXAMPLE_CALLSIGN
        );
        let uplink = format!(
            "{server}[[rbn_uplink]]\n{}",
            uncommented("rbn_uplink", |_| true)
        );
        assert_eq!(loaded(&uplink).rbn_uplink.len(), 1);

        for kind in [
            InputKind::Audio,
            InputKind::File,
            InputKind::Kiwi,
            InputKind::Soapy,
            InputKind::Hpsdr,
        ] {
            let body = format!(
                "[input]\ntype = \"{}\"\n{}",
                kind.name(),
                uncommented("input", |k| kind.keys().contains(&k))
            );
            let l = loaded(&body);
            assert_eq!(l.input.source.map(|s| s.kind()), Some(kind), "{body}");
        }
        let shared = [
            "freq_correction_ppm",
            "center_freq_hz",
            "capture_rate_hz",
            "replay_epoch",
        ];
        let body = format!("[input]\n{}", uncommented("input", |k| shared.contains(&k)));
        let l = loaded(&body);
        assert!(l.input.shared.center_freq_hz.is_some(), "{body}");
        assert!(l.input.shared.replay_epoch.is_some(), "{body}");

        let body = format!("[spot]\n{}", uncommented("spot", |_| true));
        let l = loaded(&body);
        assert!(l.spot.blocklist_path.is_some() && l.spot.notch_path.is_some());
        assert!(l.spot.cty_path.is_some() && l.spot.scp_path.is_some());
    }

    #[test]
    fn every_setting_line_is_explained() {
        for (table, lines) in sections() {
            for (key, _, n) in lines {
                let this = scaffold_line(n);
                let trailing = this[1..].contains(" # ");
                let above = n > 0 && scaffold_line(n - 1).starts_with("# ");
                assert!(
                    trailing || above,
                    "{table}.{key} (line {}) has no explanation: {this:?}",
                    n + 1
                );
            }
        }
    }

    /// The `# ` comment lines directly above scaffold line `n`, joined with
    /// a space so a phrase that wraps across two lines still matches.
    fn comment_above(n: usize) -> String {
        let mut lines = Vec::new();
        let mut i = n;
        while i > 0 && scaffold_line(i - 1).starts_with("# ") {
            i -= 1;
            lines.push(&scaffold_line(i)[2..]);
        }
        lines.reverse();
        lines.join(" ")
    }

    /// MAN-132 scenario 2: the metrics exposure choice is explicit in the
    /// generated file, and bind_addr's comment no longer claims metrics.
    #[test]
    fn scaffold_makes_the_metrics_exposure_choice_explicit() {
        assert_eq!(
            scaffold_setting("server", "metrics_bind_addr"),
            r#"metrics_bind_addr = "127.0.0.1""#
        );
        let (_, _, m) = setting_lines("server")
            .into_iter()
            .find(|(k, _, _)| k == "metrics_bind_addr")
            .unwrap();
        let why = comment_above(m);
        for needle in [
            "no password",
            "127.0.0.1",
            "this machine only",
            "\"0.0.0.0\"",
            "firewall",
        ] {
            assert!(
                why.contains(needle),
                "metrics_bind_addr comment lacks {needle:?}: {why}"
            );
        }
        let (_, _, b) = setting_lines("server")
            .into_iter()
            .find(|(k, _, _)| k == "bind_addr")
            .unwrap();
        let bind = comment_above(b);
        assert!(
            !bind.contains("metrics") && !bind.contains("all three"),
            "{bind}"
        );
    }

    #[test]
    fn list_categories_name_only_scaffold_keys() {
        for (table, key) in NO_DEFAULT.iter().chain(BUILTIN_EQUIVALENT) {
            assert!(
                setting_lines(table).iter().any(|(k, _, _)| k == key),
                "{table}.{key} is classified but the scaffold does not list it"
            );
        }
    }

    #[test]
    fn placeholder_values_are_rejected_by_check() {
        let err_of = |body: &str| format!("{:#}", reject_placeholders(&loaded(body)).unwrap_err());
        let err = err_of("[server]\nstation_callsign = \"N0CALL\"\n");
        assert!(
            err.contains(
                "server.station_callsign is still the example \"N0CALL\" -- set your own callsign"
            ),
            "{err}"
        );
        let err =
            err_of("[input]\ntype = \"kiwi\"\nhost = \"<your-receiver-host>\"\nfreq_hz = 7e6\n");
        assert!(
            err.contains(
                "input.host is still a placeholder (\"<your-receiver-host>\") -- replace it"
            ),
            "{err}"
        );
        let err = err_of(
            "[server]\nstation_callsign = \"W1AW\"\n[[rbn_uplink]]\nenabled = true\n\
             target_host = \"<collector-host>\"\ntarget_port = 7000\n",
        );
        assert!(
            err.contains("rbn_uplink.target_host is still a placeholder (\"<collector-host>\")"),
            "{err}"
        );
        let err = err_of(
            "[server]\nstation_callsign = \"W1AW\"\n[[rbn_uplink]]\nenabled = true\n\
             target_host = \"rbn.example.org\"\ntarget_port = 7000\nlogin_callsign = \"n0call\"\n",
        );
        assert!(
            err.contains("rbn_uplink.login_callsign is still the example \"N0CALL\""),
            "{err}"
        );
        let err = err_of(
            "[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\npassword = \"<s3cret>\"\n",
        );
        assert!(
            err.contains("input.password is still a placeholder -- replace it"),
            "{err}"
        );
        assert!(!err.contains("s3cret"), "the password leaked: {err}");
        let ok = loaded(
            "[server]\nstation_callsign = \"W1AW\"\n[[rbn_uplink]]\nenabled = true\n\
             target_host = \"rbn.example.org\"\ntarget_port = 7000\n\
             [input]\ntype = \"kiwi\"\nhost = \"rx.example.org\"\nfreq_hz = 7e6\n",
        );
        assert!(reject_placeholders(&ok).is_ok());
    }

    /// MAN-268: `run`'s check is the callsign half of `reject_placeholders`
    /// with the same diagnostics, and nothing else.
    #[test]
    fn reject_example_callsigns_checks_only_the_callsigns() {
        const STATION: &str =
            "server.station_callsign is still the example \"N0CALL\" -- set your own callsign";
        for call in ["N0CALL", "n0call"] {
            let l = loaded(&format!("[server]\nstation_callsign = \"{call}\"\n"));
            let err = format!("{:#}", reject_example_callsigns(&l).unwrap_err());
            assert_eq!(err, format!("{}: {STATION}", l.origin));
            assert_eq!(format!("{:#}", reject_placeholders(&l).unwrap_err()), err);
        }
        // An uncommented example uplink login is refused too: no flag
        // replaces [[rbn_uplink]].
        let uplink = "[server]\nstation_callsign = \"W1AW\"\n[[rbn_uplink]]\nenabled = true\n\
                      target_host = \"rbn.example.org\"\ntarget_port = 7000\n";
        let l = loaded(&format!("{uplink}login_callsign = \"N0CALL\"\n"));
        let err = format!("{:#}", reject_example_callsigns(&l).unwrap_err());
        assert!(
            err.contains("rbn_uplink.login_callsign is still the example \"N0CALL\""),
            "{err}"
        );
        assert_eq!(format!("{:#}", reject_placeholders(&l).unwrap_err()), err);
        assert!(reject_example_callsigns(&loaded(uplink)).is_ok());
        // A placeholder `config check` refuses but `run` leaves alone: a
        // receiver table a source flag may replace.
        let l = loaded(
            "[server]\nstation_callsign = \"W1AW\"\n\
             [input]\ntype = \"kiwi\"\nhost = \"<your-receiver-host>\"\nfreq_hz = 7e6\n",
        );
        assert!(reject_placeholders(&l).is_err());
        assert!(reject_example_callsigns(&l).is_ok());
        for body in ["", "[server]\nstation_callsign = \"W5AU-1\"\n"] {
            assert!(reject_example_callsigns(&loaded(body)).is_ok(), "{body:?}");
        }
    }

    #[test]
    fn init_refuses_to_replace_a_file_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        init(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SCAFFOLD);
        std::fs::write(&path, "sentinel").unwrap();
        let err = format!("{:#}", init(&path, false).unwrap_err());
        assert!(
            err.contains("already exists") && err.contains("--force"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sentinel");
        init(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SCAFFOLD);
    }
}
