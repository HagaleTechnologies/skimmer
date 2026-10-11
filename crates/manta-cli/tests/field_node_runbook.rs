//! MAN-96: the secondary-skimmer field-node kit (docs/RUNBOOKS/field-node/ and
//! docs/RUNBOOKS/secondary-skimmer-field-node.md) stays loadable and in step with the code:
//! the example config validates, the systemd units restart forever and outlast the SIGTERM
//! drain, the companion units call subcommands and flags `scripts/field-node.py` really has,
//! and the runbook cites only metrics `/metrics` renders and links only files that exist.

use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const KIT_DIR: &str = "docs/RUNBOOKS/field-node";
const RUNBOOK: &str = "docs/RUNBOOKS/secondary-skimmer-field-node.md";
const FIELD_NODE_PY: &str = "scripts/field-node.py";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn doc(rel: &str) -> String {
    let p = repo_root().join(rel);
    fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
        // Windows checkouts may convert LF to CRLF; the tests below parse LF-delimited
        // line structure.
        .replace("\r\n", "\n")
}

fn kit(name: &str) -> String {
    doc(&format!("{KIT_DIR}/{name}"))
}

/// A systemd unit file as `(section, key, value)` triples, in file order. Comments
/// (`#`, `;`) and blank lines are dropped; backslash continuations are joined.
fn unit_entries(text: &str) -> Vec<(String, String, String)> {
    let mut joined: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(head) = trimmed.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(trimmed);
        joined.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        joined.push(pending);
    }

    let mut section = String::new();
    let mut out = Vec::new();
    for line in joined {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_string();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.push((
                section.clone(),
                key.trim().to_string(),
                value.trim().to_string(),
            ));
        }
    }
    out
}

/// The last value of `key` in `[section]` (systemd's last-assignment-wins rule).
fn unit_value(entries: &[(String, String, String)], section: &str, key: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|(s, k, _)| s == section && k == key)
        .map(|(_, _, v)| v.clone())
}

/// Splits a systemd `ExecStart` command line into words, honouring double and single
/// quotes the way systemd (and a POSIX shell) does for these simple cases.
fn split_command_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    assert!(quote.is_none(), "unterminated quote in {line:?}");
    if in_word {
        words.push(current);
    }
    words
}

fn exec_start_words(entries: &[(String, String, String)], unit: &str) -> Vec<String> {
    let exec = unit_value(entries, "Service", "ExecStart")
        .unwrap_or_else(|| panic!("{unit} has no [Service] ExecStart"));
    // Strip systemd's executable-prefix characters (`-`, `@`, `+`, `!`, `:`).
    let exec = exec.trim_start_matches(['-', '@', '+', '!', ':']);
    split_command_line(exec)
}

/// `const SHUTDOWN_DRAIN_DEADLINE: ... from_secs(N)` in crates/manta-cli/src/main.rs.
fn shutdown_drain_deadline_secs() -> u64 {
    let src = doc("crates/manta-cli/src/main.rs");
    let line = src
        .lines()
        .find(|l| l.trim_start().starts_with("const SHUTDOWN_DRAIN_DEADLINE:"))
        .expect("main.rs defines `const SHUTDOWN_DRAIN_DEADLINE:`");
    let start = line
        .find("from_secs(")
        .unwrap_or_else(|| panic!("SHUTDOWN_DRAIN_DEADLINE is not `from_secs(N)`: {line}"))
        + "from_secs(".len();
    let len = line[start..]
        .find(')')
        .expect("from_secs( is closed with `)`");
    line[start..start + len]
        .trim()
        .replace('_', "")
        .parse()
        .unwrap_or_else(|e| panic!("SHUTDOWN_DRAIN_DEADLINE seconds in {line:?}: {e}"))
}

/// A systemd time span in whole seconds: a bare number, or `Ns` / `Nmin` / `Nm`.
fn systemd_seconds(value: &str) -> u64 {
    let v = value.trim();
    let parse = |n: &str| -> u64 {
        n.trim()
            .parse()
            .unwrap_or_else(|e| panic!("time span {value:?}: {e}"))
    };
    if let Some(n) = v.strip_suffix("min") {
        parse(n) * 60
    } else if let Some(n) = v.strip_suffix('m') {
        parse(n) * 60
    } else if let Some(n) = v.strip_suffix('s') {
        parse(n)
    } else {
        parse(v)
    }
}

#[test]
fn example_config_validates_feature_aware() {
    let config = repo_root().join(KIT_DIR).join("manta-field.toml");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    // `config check` reads `MANTA_CONFIG` and `MANTA_<TABLE>_<KEY>` and rejects unknown
    // `MANTA_*` names, so the test runner's own environment must not leak into the child.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    let out = cmd
        .args(["config", "check", "--config"])
        .arg(&config)
        .output()
        .expect("run manta config check");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if cfg!(feature = "soapy") {
        assert_eq!(
            out.status.code(),
            Some(0),
            "a soapy build must accept the example config\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    } else {
        assert_eq!(
            out.status.code(),
            Some(1),
            "a build without soapy must reject only the soapy input\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains(r#"input.type = "soapy" needs a manta built with --features soapy"#),
            "every other table must validate before the soapy feature check\nstderr:\n{stderr}"
        );
    }
}

#[test]
fn example_config_is_a_secondary_skimmer_config() {
    let text = kit("manta-field.toml");
    let table: toml::Table = toml::from_str(&text).expect("manta-field.toml parses as TOML");

    let server = table
        .get("server")
        .and_then(|v| v.as_table())
        .expect("manta-field.toml has a [server] table");
    assert_eq!(
        server.get("line_format").and_then(|v| v.as_str()),
        Some("skimmer"),
        "a secondary skimmer behind an Aggregator uses CW Skimmer's line layout"
    );
    let interval = server
        .get("status_interval_secs")
        .and_then(|v| v.as_integer())
        .expect("server.status_interval_secs is set");
    assert!(interval > 0, "status lines must stay on for the field log");
    let call = server
        .get("station_callsign")
        .and_then(|v| v.as_str())
        .expect("server.station_callsign is set");
    assert!(
        call.contains('-'),
        "station_callsign {call:?} needs an RBN SSID (MAN-89)"
    );

    assert!(
        !table.contains_key("rbn_uplink"),
        "the Aggregator forwards a secondary skimmer's spots; the node has no [[rbn_uplink]] (D-L)"
    );

    let input = table
        .get("input")
        .and_then(|v| v.as_table())
        .expect("manta-field.toml has an [input] table");
    assert!(
        input.get("type").and_then(|v| v.as_str()).is_some(),
        "input.type must name the receiver"
    );
}

#[test]
fn manta_unit_restarts_forever_and_outlasts_the_drain() {
    let entries = unit_entries(&kit("manta-field.service"));

    assert_eq!(
        unit_value(&entries, "Unit", "StartLimitIntervalSec").as_deref(),
        Some("0"),
        "systemd must never give up restarting manta"
    );
    assert_eq!(
        unit_value(&entries, "Service", "Restart").as_deref(),
        Some("always")
    );

    let drain = shutdown_drain_deadline_secs();
    let stop = systemd_seconds(
        &unit_value(&entries, "Service", "TimeoutStopSec")
            .expect("manta-field.service sets TimeoutStopSec"),
    );
    assert!(
        stop >= drain + 2 + 5,
        "TimeoutStopSec={stop} must cover SHUTDOWN_DRAIN_DEADLINE ({drain} s) + 2 s runtime \
         shutdown + 5 s margin, or systemd SIGKILLs a draining daemon"
    );

    let words = exec_start_words(&entries, "manta-field.service");
    let program = words.first().expect("ExecStart names a program");
    assert!(
        program == "manta" || program.ends_with("/manta"),
        "ExecStart runs manta, not {program:?}"
    );
    assert_eq!(
        words.get(1..3),
        Some(&["run".to_string(), "--config".to_string()][..]),
        "ExecStart runs `manta run --config <file>`: {words:?}"
    );
    let config = words.get(3).expect("ExecStart names a config file");
    assert_eq!(config, "/etc/manta/manta-field.toml");
    let basename = Path::new(config)
        .file_name()
        .and_then(|n| n.to_str())
        .expect("config path has a file name");
    assert!(
        repo_root().join(KIT_DIR).join(basename).is_file(),
        "ExecStart's config {basename} must be the TOML shipped in {KIT_DIR}"
    );
}

#[test]
fn companion_units_invoke_existing_subcommands_and_flags() {
    let script = doc(FIELD_NODE_PY);
    for unit in ["manta-field-ledger.service", "manta-field-spots.service"] {
        let entries = unit_entries(&kit(unit));
        assert_eq!(
            unit_value(&entries, "Service", "Restart").as_deref(),
            Some("always"),
            "{unit} must restart forever"
        );

        let words = exec_start_words(&entries, unit);
        let at = words
            .iter()
            .position(|w| w.ends_with("field-node.py"))
            .unwrap_or_else(|| panic!("{unit}'s ExecStart does not run field-node.py: {words:?}"));
        let sub = words
            .get(at + 1)
            .unwrap_or_else(|| panic!("{unit} names no field-node.py subcommand"));
        assert!(
            script.contains(&format!("add_parser(\"{sub}\"")),
            "{unit} runs `field-node.py {sub}`, which {FIELD_NODE_PY} does not define"
        );
        // Quoted values (the --recover-cmd command line) are single words here, so their
        // own dashes never look like field-node.py flags.
        for word in &words[at + 2..] {
            if let Some(flag) = word.strip_prefix("--") {
                let flag = flag.split('=').next().unwrap_or(flag);
                assert!(
                    script.contains(&format!("\"--{flag}\"")),
                    "{unit} passes --{flag}, which {FIELD_NODE_PY} does not define"
                );
            }
        }

        if unit == "manta-field-ledger.service" {
            let recover = words
                .iter()
                .position(|w| w == "--recover-cmd" || w.starts_with("--recover-cmd="))
                .map(|i| {
                    words[i]
                        .strip_prefix("--recover-cmd=")
                        .map(str::to_string)
                        .or_else(|| words.get(i + 1).cloned())
                        .unwrap_or_default()
                })
                .expect("the ledger unit passes --recover-cmd");
            assert!(
                recover.contains("manta-field-recover"),
                "--recover-cmd must run manta-field-recover, not {recover:?}"
            );
        }
    }
}

#[test]
fn recover_script_restarts_the_manta_unit() {
    let script = kit("manta-field-recover");
    assert!(
        script.starts_with("#!/bin/sh\n"),
        "manta-field-recover is a POSIX sh script"
    );
    assert!(
        script.lines().any(|l| l.trim() == "set -eu"),
        "manta-field-recover must `set -eu`"
    );
    assert!(
        script.contains("systemctl restart manta-field.service"),
        "manta-field-recover must restart manta-field.service"
    );
}

#[test]
fn runbook_cites_only_rendered_metrics() {
    let runbook = doc(RUNBOOK);
    let metrics_src = doc("crates/manta-server/src/metrics.rs");
    let token = Regex::new(r"\bmanta_[a-z0-9_]+").unwrap();
    let mut missing = Vec::new();
    let mut seen = 0;
    for m in token.find_iter(&runbook) {
        let raw = m.as_str();
        let name = ["_bucket", "_sum", "_count"]
            .iter()
            .find_map(|s| raw.strip_suffix(s))
            .unwrap_or(raw);
        seen += 1;
        if !metrics_src.contains(&format!("# TYPE {name} ")) {
            missing.push(raw.to_string());
        }
    }
    assert!(seen > 0, "{RUNBOOK} cites no manta_* metric at all");
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "{RUNBOOK} cites metrics crates/manta-server/src/metrics.rs does not render: {missing:?}"
    );
}

#[test]
fn runbook_relative_links_resolve() {
    let runbook = doc(RUNBOOK);
    let base = repo_root()
        .join(RUNBOOK)
        .parent()
        .expect("runbook has a directory")
        .to_path_buf();
    let link = Regex::new(r#"\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#).unwrap();
    let mut broken = Vec::new();
    for cap in link.captures_iter(&runbook) {
        let target = &cap[1];
        if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("mailto:")
            || target.starts_with('#')
        {
            continue;
        }
        let path = target.split('#').next().unwrap_or(target);
        if !base.join(path).exists() {
            broken.push(target.to_string());
        }
    }
    assert!(
        broken.is_empty(),
        "{RUNBOOK} links to files that do not exist (relative to {}): {broken:?}",
        base.display()
    );
}

#[test]
fn runbook_names_every_stage_and_both_scenarios() {
    let runbook = doc(RUNBOOK);
    // A `#` line inside a fenced code block is a shell comment, not a heading.
    let mut in_fence = false;
    let headings: Vec<&str> = runbook
        .lines()
        .filter(|l| {
            if l.trim_start().starts_with("```") {
                in_fence = !in_fence;
                return false;
            }
            !in_fence && l.starts_with('#')
        })
        .collect();
    for needle in ["Stage 0", "Stage 1", "Stage 2", "Day 7", "Day 30"] {
        assert!(
            headings.iter().any(|h| h.contains(needle)),
            "{RUNBOOK} has no heading naming {needle:?}; headings: {headings:#?}"
        );
    }
    assert!(
        runbook.to_lowercase().contains("go/no-go"),
        "{RUNBOOK} must state the Stage 1 go/no-go decision"
    );
}

/// MAN-127: Stage 1's frequency calibration offers `manta calibrate` against a
/// known carrier, and keeps `shadow-compare`'s formula as the cross-check and
/// the fallback when no reference is audible.
#[test]
fn runbook_offers_manta_calibrate_for_stage_1() {
    let runbook = doc(RUNBOOK);
    let start = runbook
        .find("\n## 5. Stage 1")
        .expect("the runbook has a `## 5. Stage 1` section");
    let stage1 = &runbook[start + 1..];
    let stage1 = &stage1[..stage1.find("\n## ").unwrap_or(stage1.len())];
    let at = stage1
        .find("Frequency calibration")
        .expect("Stage 1 has a frequency-calibration paragraph");
    let calibration = &stage1[at..];
    let calibration = &calibration[..calibration
        .find("On **NO-GO**")
        .unwrap_or(calibration.len())];
    for needle in [
        "manta calibrate --config /etc/manta/manta-field.toml",
        "shadow-compare",
        "freq_correction_ppm = -(median Δf) / freq_hz × 1e6",
    ] {
        assert!(
            calibration.contains(needle),
            "{RUNBOOK}'s Stage 1 frequency calibration never says {needle:?}"
        );
    }
    let calibrate = calibration.find("manta calibrate").unwrap();
    let formula = calibration.find("freq_correction_ppm = -(median").unwrap();
    assert!(
        calibrate < formula,
        "{RUNBOOK} must offer `manta calibrate` before the shadow-compare fallback"
    );
}
