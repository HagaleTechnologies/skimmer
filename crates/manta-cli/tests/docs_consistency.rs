//! The public docs must match the shipped binary. MAN-145.
//!
//! These assertions exist because README.md, ARCHITECTURE.md, CLAUDE.md and
//! wiki/pages/overview.md independently drifted to three different crate
//! counts and a two-milestone-stale description of the output layer. Each
//! test below pins one claim a first-time visitor reads.

use manta_dsp::channelizer::Channelizer;
use std::fs;
use std::path::{Path, PathBuf};

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
        // frontmatter and line structure.
        .replace("\r\n", "\n")
}

/// Every run of whitespace collapsed to a single space, so a phrase that a
/// Markdown reflow split across two lines still matches.
fn squash_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text between a `## <heading>` and the next `## ` heading.
fn section<'a>(md: &'a str, heading: &str) -> &'a str {
    let start = md
        .find(&format!("\n## {heading}"))
        .unwrap_or_else(|| panic!("no `## {heading}` section"));
    let rest = &md[start + 1..];
    let end = rest[3..].find("\n## ").map(|i| i + 4).unwrap_or(rest.len());
    &rest[..end]
}

fn workspace_members() -> Vec<String> {
    let manifest: toml::Value = toml::from_str(&doc("Cargo.toml")).expect("parse Cargo.toml");
    manifest["workspace"]["members"]
        .as_array()
        .expect("members array")
        .iter()
        .map(|m| {
            m.as_str()
                .expect("member string")
                .rsplit('/')
                .next()
                .expect("crate name")
                .to_string()
        })
        .collect()
}

// ---- Scenario 3: the crate count is accurate everywhere ----

/// Every crate in the workspace must be named somewhere in ARCHITECTURE.md
/// (the assertion searches the whole document, not only the crate tree --
/// a crate dropped from the tree but still named in prose passes here, and
/// that is deliberate: prose mentions are a legitimate home for a crate the
/// tree does not draw). `manta-soak-harness` was named nowhere at all for
/// the whole of M2/M3.
#[test]
fn architecture_lists_every_workspace_crate() {
    let arch = doc("ARCHITECTURE.md");
    for krate in workspace_members() {
        assert!(
            arch.contains(&krate),
            "ARCHITECTURE.md never mentions workspace member `{krate}`"
        );
    }
}

/// README, CLAUDE.md and the wiki overview must all state the same crate
/// count, and it must be the real one. ARCHITECTURE.md is deliberately not
/// in the list: it states no count in words, and is covered for crate
/// *names* by `architecture_lists_every_workspace_crate` above. Guard-
/// listing the stale spellings keeps the failure message actionable when a
/// crate is added.
#[test]
fn every_doc_states_the_real_crate_count() {
    let n = workspace_members().len();
    assert_eq!(
        n, 9,
        "crate count changed to {n}: update the docs below and this test"
    );

    let stale = [
        "seven-crate",
        "eight-crate",
        "8-crate",
        "7-crate",
        "six-crate",
    ];
    let fresh = ["nine-crate", "9-crate"];

    for file in ["README.md", "CLAUDE.md", "wiki/pages/overview.md"] {
        let text = doc(file);
        for s in stale {
            assert!(
                !text.contains(s),
                "{file} still says `{s}` (workspace has {n} crates)"
            );
        }
        assert!(
            fresh.iter().any(|f| text.contains(f)),
            "{file} states no crate count; expected one of {fresh:?}"
        );
    }
}

// ---- Scenario 2: the Inputs table lists every shipped input ----

/// Every input the CLI can be built with needs a row. HPSDR/Hermes shipped
/// (crates/manta-input/src/hpsdr.rs, feature `hpsdr`, which the README's
/// install line turns on) and had no row at all.
#[test]
fn readme_inputs_table_covers_every_shipped_input() {
    let readme = doc("README.md");
    let inputs = squash_whitespace(section(&readme, "Inputs"));
    for needle in [
        "WAV file",
        "--device",
        "--kiwi-host",
        "--soapy-driver",
        "--hpsdr-host",
    ] {
        assert!(
            inputs.contains(needle),
            "README Inputs table has no row for `{needle}`"
        );
    }
}

// ---- Scenario 1: Outputs and Status describe shipped capability ----

/// The telnet server, JSON/WebSocket stream, RBN uplink and Prometheus
/// endpoint have all shipped and are covered by manta-server's test suite.
#[test]
fn readme_outputs_describes_shipped_servers() {
    let readme = doc("README.md");
    // Squashed like the ROADMAP guard below: a reflow that split one of
    // these phrases across two lines would otherwise disable the assertion
    // silently instead of reporting real drift.
    let outputs = squash_whitespace(section(&readme, "Outputs"));
    assert!(
        !outputs.contains("in progress"),
        "README Outputs still calls a shipped server \"in progress\""
    );
    for needle in ["7300", "7301", "7302", "uplink", "--config"] {
        assert!(
            outputs.contains(needle),
            "README Outputs never mentions `{needle}`"
        );
    }
}

/// MAN-123: README Outputs says where `run`'s human output goes.
#[test]
fn readme_outputs_says_where_run_prints_spots_and_decoded_text() {
    let outputs = squash_whitespace(section(&doc("README.md"), "Outputs"));
    for needle in ["stdout", "stderr", "one line per track", "--decoded-text"] {
        assert!(
            outputs.contains(needle),
            "README Outputs never mentions `{needle}`"
        );
    }
}

/// The Status block must not list shipped work as upcoming.
#[test]
fn readme_status_does_not_promise_shipped_work() {
    let readme = doc("README.md");
    let status = squash_whitespace(section(&readme, "Status"));
    for stale in ["the telnet and JSON spot servers", "TOML config, metrics"] {
        assert!(
            !status.contains(stale),
            "README Status still lists `{stale}` as future work"
        );
    }
}

/// Every command in the Quickstart must run on a build the reader was told
/// to make. `--soapy-driver` needs `--features soapy`, which is in neither
/// the default build nor any release binary.
#[test]
fn readme_quickstart_only_uses_default_build_flags() {
    let readme = doc("README.md");
    let quickstart = squash_whitespace(section(&readme, "60-second demo"));
    for gated in ["--soapy-driver", "--soapy-freq", "--soapy-rate"] {
        assert!(
            !quickstart.contains(gated),
            "60-second demo uses feature-gated flag `{gated}`; it fails with \
             `error: unexpected argument` on a default or release build"
        );
    }
}

// ---- Scenario 4: ROADMAP's RBN-admission item ----

/// RBN admission is active P0 work (MAN-40; decision D1, 2026-09-06), not a
/// deferred post-1.0 idea.
#[test]
fn roadmap_does_not_defer_rbn_admission() {
    let roadmap = doc("ROADMAP.md");
    let post = squash_whitespace(section(&roadmap, "Post-1.0 candidates"));
    assert!(
        !post.contains("RBN operators"),
        "ROADMAP still defers RBN-operator admission to post-1.0"
    );
}

/// M3's own prose must not list the shipped manta-server as remaining.
#[test]
fn roadmap_m3_does_not_list_shipped_server_as_remaining() {
    let roadmap = doc("ROADMAP.md");
    // Collapse runs of whitespace so the search survives a paragraph reflow:
    // the phrase is line-wrapped in the source Markdown, and matching the raw
    // text would panic on `.expect()` instead of reporting a real drift.
    let m3 = squash_whitespace(section(&roadmap, "M3"));
    let idx = m3
        .find("Remaining M3 sub-projects")
        .expect("M3 remaining-work sentence");
    assert!(
        !m3[idx..].contains("manta-server"),
        "ROADMAP M3 still lists `manta-server` as a remaining sub-project"
    );
}

// ---- Runnable-demo hygiene: no unmarked placeholder receivers ----

/// Lines inside ```-fenced blocks, so prose and table cells (which name
/// `--kiwi-host` without an argument) are not mistaken for commands.
fn fenced_lines(md: &str) -> Vec<&str> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            inside = !inside;
            continue;
        }
        if inside {
            out.push(line);
        }
    }
    out
}

/// A command a newcomer is invited to copy must not carry a hostname that
/// can never resolve to a receiver. The demo shipped `--kiwi-host
/// kiwi.example.org` -- reserved by RFC 2606, so the advertised
/// hardware-free live path dead-ends on a connection error with nothing in
/// the README saying the host was yours to supply. Every `--kiwi-host`
/// argument in a copyable block must be an angle-bracket placeholder, and
/// the demo must point at somewhere real receivers are listed.
#[test]
fn readme_kiwi_commands_use_a_marked_placeholder_host() {
    let readme = doc("README.md");
    for line in fenced_lines(&readme) {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok != "--kiwi-host" {
                continue;
            }
            let host = toks
                .next()
                .unwrap_or_else(|| panic!("`--kiwi-host` with no argument in: {line}"));
            assert!(
                host.starts_with('<') && host.ends_with('>'),
                "README command uses `--kiwi-host {host}`; a copyable command needs an \
                 explicit `<placeholder>` the reader must replace, not a host that \
                 cannot connect"
            );
        }
    }

    let demo = squash_whitespace(section(&readme, "60-second demo"));
    assert!(
        demo.contains("kiwisdr.com/public") || demo.contains("rx.linkfanel.net"),
        "60-second demo names a KiwiSDR placeholder but points nowhere the reader \
         can find a real public receiver"
    );
}

// ---- Review round 1 (PR #142): the documented commands must stay coherent ----

/// Fenced lines with shell line-continuations joined, so a command split
/// across two lines with a trailing `\` is still inspected as one command.
fn fenced_commands(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in fenced_lines(md) {
        let trimmed = line.trim_end();
        if let Some(head) = trimmed.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(trimmed);
        out.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

/// Every documented way of building the CLI must produce the same binary the
/// rest of the README describes. `manta-cli` declares no default features and
/// gates `--hpsdr-host` behind `hpsdr`, so a build command without
/// `--features hpsdr` yields a binary that rejects the input flag the
/// Installation notes and the Inputs table both advertise.
#[test]
fn readme_build_commands_keep_the_hpsdr_feature() {
    let readme = doc("README.md");
    for line in readme.lines() {
        if !line.contains("manta-cli") {
            continue;
        }
        if !(line.contains("cargo build") || line.contains("cargo install")) {
            continue;
        }
        assert!(
            line.contains("--features hpsdr"),
            "README builds manta-cli without `--features hpsdr`, so the binary has no \
             `--hpsdr-host` flag: {line}"
        );
    }
}

/// The Kiwi prose tells the reader to substitute the receiver's hostname
/// *and port*, and `--kiwi-port` defaults to 8073 in silence -- so a command
/// that offers only a host placeholder cannot reach a receiver on any other
/// port. Both copyable Kiwi commands need a marked port placeholder too.
#[test]
fn readme_kiwi_commands_offer_a_marked_placeholder_port() {
    let readme = doc("README.md");
    for cmd in fenced_commands(&readme) {
        if !cmd.contains("--kiwi-host") {
            continue;
        }
        let mut toks = cmd.split_whitespace();
        let mut port = None;
        while let Some(tok) = toks.next() {
            if tok == "--kiwi-port" {
                port = toks.next();
            }
        }
        let port = port.unwrap_or_else(|| {
            panic!(
                "README Kiwi command has no `--kiwi-port`, so it silently uses the 8073 \
                 default the surrounding prose tells the reader to replace: {cmd}"
            )
        });
        assert!(
            port.starts_with('<') && port.ends_with('>'),
            "README command uses `--kiwi-port {port}`; the reader must be shown a \
             `<placeholder>` to replace, not a fixed port"
        );
    }
}

/// M3's remaining-work prose must not quote a subset of the accept-when list.
/// It previously named the 7-day soak but not the stock-DX-cluster-client
/// session, which this branch's own verification notes leave open -- making
/// M3 read as closer to acceptance than it is.
#[test]
fn roadmap_m3_remaining_work_names_every_open_acceptance_gate() {
    let roadmap = doc("ROADMAP.md");
    let m3 = squash_whitespace(section(&roadmap, "M3"));
    let idx = m3
        .find("Remaining M3 sub-projects")
        .expect("M3 remaining-work sentence");
    let remaining = &m3[idx..];
    for gate in [
        "parity benchmark",
        "7-day unattended soak",
        "cqdx",
        "stock DX-cluster client",
    ] {
        assert!(
            remaining.contains(gate),
            "ROADMAP M3's remaining-work prose never mentions the open accept-when gate \
             `{gate}`"
        );
    }
}

/// The input rates of the normative rate table in `docs/SPEC-decode-core.md`
/// §1.1, in kS/s (`96`, `192`, `384`, `768`). The README's supported-rate
/// list must name every one of them: a rate the SPEC admits but the README
/// omits reads to a visitor as "your recording is unsupported" when the
/// channelizer would in fact have accepted it.
fn spec_table_rates_ks() -> Vec<u32> {
    let spec = doc("docs/SPEC-decode-core.md");
    let start = spec.find("### 1.1 Dimensions").expect("SPEC §1.1 heading");
    let body = &spec[start..];
    let end = body[1..]
        .find("\n### ")
        .map(|i| i + 1)
        .unwrap_or(body.len());
    let mut rates = Vec::new();
    for line in body[..end].lines() {
        let Some(cell) = line.strip_prefix('|').and_then(|l| l.split('|').next()) else {
            continue;
        };
        let digits: String = cell.chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(hz) = digits.parse::<u32>() {
            rates.push(hz / 1000);
        }
    }
    assert!(
        rates.contains(&768),
        "SPEC §1.1 table parsed as {rates:?}; expected it to name 768 kS/s"
    );
    rates
}

/// `--source-iq` only changes how the WAV's samples are *interpreted*; it is
/// not a resampler. `Channelizer::new` rejects any rate whose `fs / 93.75`
/// is not a power of two (`crates/manta-dsp/src/channelizer.rs`), and
/// `--capture-rate-hz` only decimates by a power of two into another table
/// rate (`manta_dsp::decimate::Decimator::new`) -- so a 100 or 250 kS/s
/// recording cannot be replayed at all, by either route. The README must
/// therefore not promise IQ replay "at any rate", and must name the table
/// constraint wherever it describes IQ replay.
#[test]
fn readme_does_not_promise_iq_replay_at_any_rate() {
    let readme = squash_whitespace(&doc("README.md"));
    assert!(
        !readme.contains("at any rate"),
        "README promises IQ file replay `at any rate`, but the channelizer only accepts \
         rates where fs/93.75 is a power of two and --capture-rate-hz only decimates by a \
         power of two -- a 100 kS/s recording is rejected outright"
    );
    // ...nor may it frame the rates it *does* name as the closed set. The
    // rule admits every `fs` with `fs / 93.75` a power of two, which is
    // unbounded in both directions (24 kS/s and 1536 kS/s both construct);
    // "any other rate is rejected" reads to a reader holding one of the
    // unlisted-but-valid rates as a flat no.
    assert!(
        !readme.contains("at any other rate"),
        "README calls every rate outside its own example list rejected, but the list is \
         not the whole set -- 24 kS/s is admitted and unlisted in SPEC §1.1's table"
    );
    assert!(
        readme.contains("fs / 93.75"),
        "README never states the channelizer's supported-rate rule (fs / 93.75 a power of \
         two), so a reader with an off-table recording has nothing to check their file \
         against"
    );
    // Each place the rule is stated must also spell out the rates it admits
    // -- the rule alone ("a power of two") makes the reader do the
    // arithmetic before they can tell whether their own recording is
    // replayable.
    // 24 and 48 kS/s are admitted rates SPEC §1.1's *table* leaves out: 48
    // kS/s is the audio-passband rate the section names in prose, and 24 kS/s
    // is the decimated rate `crates/manta-cli/tests/cli.rs`'s
    // `capture_rate_hz_that_divides_evenly_decimates_and_still_decodes`
    // already decodes end-to-end (24000 / 93.75 = 256, a power of two). The
    // rest come from the table itself.
    let mut admitted = vec![24, 48];
    admitted.extend(spec_table_rates_ks());
    admitted.dedup();
    for ks in &admitted {
        let fs = f64::from(*ks) * 1000.0;
        assert!(
            Channelizer::new(fs, 14_000_000.0).is_ok(),
            "this test claims the README must advertise {ks} kS/s, but \
             Channelizer::new rejects it"
        );
    }
    for (i, _) in readme.match_indices("fs / 93.75") {
        let window = &readme[i..readme.len().min(i + 400)];
        for ks in &admitted {
            assert!(
                window.contains(&ks.to_string()),
                "README states the fs / 93.75 rule without naming the supported table \
                 rate {ks} kS/s near it: {window}"
            );
        }
        assert!(
            window.contains("kS/s"),
            "README states the fs / 93.75 rule without units on the rates it admits: \
             {window}"
        );
    }
}

// ---- Review round 2 (PR #142): the wiki's freshness stamp must not lag ----

/// `wiki/pages/overview.md` carries a `verified: {commit, date}` frontmatter
/// stamp that readers use to judge how stale the page is. This branch
/// rewrote the page's body (nine-crate layout, "implementation is well
/// underway" replacing "no implementation has started") while the stamp
/// still pointed at `e68b106` / `2026-07-07` -- a revision predating every
/// one of those claims, so the metadata vouched for content that did not
/// exist when it was written. The in-repo precedent for a substantive
/// correction is to refresh the stamp with the change
/// (`wiki/pages/detector-tracks.md`, `wiki/pages/spot-validation.md`).
#[test]
fn wiki_overview_verified_stamp_is_not_the_pre_rewrite_one() {
    let page = doc("wiki/pages/overview.md");
    let front = page
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---").map(|(f, _)| f))
        .expect("overview.md frontmatter block");

    let field = |key: &str| -> String {
        front
            .lines()
            .find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
            .unwrap_or_else(|| panic!("overview.md frontmatter has no `{key}` field"))
    };

    // The body claims this branch introduced; if they are present the stamp
    // must have moved with them.
    let claims = ["nine-crate", "Implementation is well underway"];
    for claim in claims {
        assert!(
            page.contains(claim),
            "overview.md no longer claims `{claim}`; this test pins the stamp to those \
             claims and needs updating with them"
        );
    }

    let (commit, date) = (field("commit:"), field("date:"));
    assert_ne!(
        (commit.as_str(), date.as_str()),
        ("e68b106", "2026-07-07"),
        "overview.md still stamps `verified: e68b106 / 2026-07-07`, which predates the \
         nine-crate and shipped-status claims now on the page: refresh \
         `verified.commit`/`verified.date` whenever the body is substantively corrected"
    );
    assert!(
        date.as_str() >= "2026-09-05",
        "overview.md's `verified.date` ({date}) predates the 2026-09-05 broad review that \
         found the page's claims stale, so the stamp cannot be vouching for the current body"
    );
}

// ---- MAN-261: SPEC §9 is the loadable key table; README's node is one file ----

fn manta() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
}

/// A 15 s V1 render (fast, still one W1AW CQ), written into `dir`; returns
/// the WAV path. Same shortened spec `tests/cli.rs` uses.
fn short_v1_wav(dir: &Path) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

/// The body of the first ```toml fenced block in `md`.
fn toml_block(md: &str) -> String {
    let body: Vec<&str> = md
        .lines()
        .skip_while(|l| l.trim() != "```toml")
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with("```"))
        .collect();
    assert!(!body.is_empty(), "no ```toml fenced block found");
    body.join("\n") + "\n"
}

/// SPEC §9's key block, verbatim.
fn spec_section_9_toml() -> String {
    let spec = doc("docs/SPEC-decode-core.md");
    toml_block(section(&spec, "9. Configuration keys"))
}

/// The lines of SPEC §9's `[<table>]`: from its header up to the next
/// table header or the end of the fenced block.
fn spec_table_lines(table: &str) -> Vec<String> {
    let header = format!("[{table}]");
    spec_section_9_toml()
        .lines()
        .skip_while(|l| l.trim() != header)
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .map(str::to_string)
        .collect()
}

/// `key = …` as a live line or a commented-out `# key = …` line.
fn is_key_line(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    let t = t.strip_prefix('#').map(str::trim_start).unwrap_or(t);
    t.strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

const SERVER_TABLE: &str = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
                            telnet_port = 0\njson_port = 0\nmetrics_port = 0\n";

/// MAN-34 left `center_freq_hz` CLI-only: `DaemonConfigFile` did not model
/// `[input]`, so a daemon TOML setting it still failed the `--dial-freq-hz
/// is required with --config` guard. MAN-261 made `[input]` live -- an
/// untyped `[input]` (shared keys only) survives a CLI `--source`, so the
/// file's `center_freq_hz` now satisfies the guard and the run gets as far
/// as opening the (missing) WAV. The SPEC comment must say so, not still
/// call the key CLI-only.
#[test]
fn spec_input_center_freq_hz_is_a_live_config_key() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("daemon.toml");
    fs::write(
        &cfg,
        format!("{SERVER_TABLE}\n[input]\ncenter_freq_hz = 14030000.0\n"),
    )
    .unwrap();
    let out = manta()
        .args(["run", "--source", "/nonexistent.wav", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "`run --source /nonexistent.wav` succeeded? stderr: {stderr}"
    );
    assert!(
        !stderr.contains("--dial-freq-hz is required"),
        "[input] center_freq_hz in the config file does not satisfy the dial guard: \
         {stderr}"
    );
    assert!(
        stderr.contains("Failed to open WAV"),
        "expected the run to get past the dial guard and fail opening the WAV: {stderr}"
    );

    let input = spec_table_lines("input");
    assert!(!input.is_empty(), "SPEC §9 has no `[input]` table to check");
    let key = input
        .iter()
        .position(|l| is_key_line(l, "center_freq_hz"))
        .expect("SPEC §9 [input] no longer documents center_freq_hz");
    let mut comment: Vec<&str> = input[..key]
        .iter()
        .rev()
        .take_while(|l| l.trim_start().starts_with('#'))
        .map(String::as_str)
        .collect();
    comment.reverse();
    comment.push(&input[key]);
    let comment = comment.join(" ");
    for stale in ["CLI-only", "has no effect"] {
        assert!(
            !comment.contains(stale),
            "SPEC §9 [input] center_freq_hz comment still says `{stale}`: {comment}"
        );
    }
    assert!(
        comment.contains("--dial-freq-hz"),
        "SPEC §9 [input] center_freq_hz comment never names its `--dial-freq-hz` \
         override: {comment}"
    );
}

/// SPEC §9's block used to put two `key = value` pairs on one line, so it
/// could not be copied into a config file at all. It must now be a file
/// `manta` loads -- and since it states the defaults, loading it must change
/// nothing: `decode --json` output is byte-identical with and without it.
#[test]
fn spec_section_9_block_is_valid_toml_that_manta_loads() {
    let block = spec_section_9_toml();
    if let Err(e) = toml::from_str::<toml::Table>(&block) {
        panic!("SPEC §9's block is not valid TOML: {e}\n{block}");
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = short_v1_wav(dir.path());
    let cfg = dir.path().join("spec9.toml");
    fs::write(&cfg, &block).unwrap();

    let with = manta()
        .args(["decode", "--json", "--config"])
        .arg(&cfg)
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        with.status.success(),
        "`manta decode --config <SPEC §9 block>` failed: {}",
        String::from_utf8_lossy(&with.stderr)
    );
    let without = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        without.status.success(),
        "baseline `manta decode --json` failed: {}",
        String::from_utf8_lossy(&without.stderr)
    );
    assert!(
        !without.stdout.is_empty(),
        "baseline decode printed nothing"
    );
    assert!(
        with.stdout == without.stdout,
        "SPEC §9's block changes decode output, so its values are not the defaults\n\
         with:    {}\nwithout: {}",
        String::from_utf8_lossy(&with.stdout),
        String::from_utf8_lossy(&without.stdout)
    );
}

/// The SPEC §9 table as a parsed TOML document.
fn spec_section_9_doc() -> toml::Table {
    toml::from_str(&spec_section_9_toml()).expect("SPEC §9 block parses as TOML")
}

/// The values SPEC §9 states for `[detector]` must be the code's defaults.
/// SPEC v1 said `on_snr_db = 6.0` for months after the code moved to 12.0
/// (docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md item 2).
#[test]
fn spec_detector_values_equal_the_code_defaults() {
    let doc = spec_section_9_doc();
    let detector = doc
        .get("detector")
        .expect("SPEC §9 block has no [detector] table")
        .clone();
    let table = detector.as_table().expect("[detector] is a table");
    for key in [
        "on_snr_db",
        "off_snr_db",
        "confirm_ms",
        "hang_ms",
        "gc_ms",
        "warmup_ms",
        "track_cap",
        "silent_respawn_cooldown_ms",
    ] {
        assert!(
            table.contains_key(key),
            "SPEC §9 [detector] does not state `{key}`, so its default goes unchecked"
        );
    }
    let parsed: manta_engine::config_file::DetectorConfigToml = detector
        .try_into()
        .unwrap_or_else(|e| panic!("SPEC §9 [detector] does not parse: {e}"));
    let cfg = parsed
        .into_detector_config()
        .unwrap_or_else(|e| panic!("SPEC §9 [detector] is rejected: {e}"));
    assert_eq!(
        cfg,
        manta_engine::DetectorConfig::default(),
        "SPEC §9 [detector] values differ from DetectorConfig::default()"
    );
}

/// Same for `[decode]`. `DecodeConfig` has no `PartialEq`, so compare the
/// `Debug` rendering.
#[test]
fn spec_decode_values_equal_the_code_defaults() {
    let doc = spec_section_9_doc();
    let decode = doc
        .get("decode")
        .expect("SPEC §9 block has no [decode] table")
        .clone();
    let table = decode.as_table().expect("[decode] is a table");
    for key in [
        "engine",
        "timing_sigma",
        "beam_width",
        "debounce_ms",
        "hyst_frac",
        "tau_lo_ms",
        "tau_hi_bounds_ms",
        "flush_gap_dits",
    ] {
        assert!(
            table.contains_key(key),
            "SPEC §9 [decode] does not state `{key}`, so its default goes unchecked"
        );
    }
    let parsed: manta_decode::config_file::DecodeConfigToml = decode
        .try_into()
        .unwrap_or_else(|e| panic!("SPEC §9 [decode] does not parse: {e}"));
    assert_eq!(
        format!("{:?}", parsed.into_decode_config()),
        format!("{:?}", manta_decode::decoder::DecodeConfig::default()),
        "SPEC §9 [decode] values differ from DecodeConfig::default()"
    );
}

/// Every `[input]` and `[spot]` key the loader accepts has a line (live or
/// commented out) in SPEC §9, so the SPEC is the one place to look a key up.
#[test]
fn spec_documents_every_input_and_spot_key() {
    let tables: [(&str, &[&str]); 2] = [
        (
            "input",
            &[
                "type",
                "device",
                "path",
                "iq",
                "host",
                "port",
                "freq_hz",
                "password",
                "driver",
                "rate_hz",
                "gain_db",
                "freq_correction_ppm",
                "center_freq_hz",
                "capture_rate_hz",
                "replay_epoch",
            ],
        ),
        (
            "spot",
            &[
                "allowlist",
                "blocklist_path",
                "notch_path",
                "cty_path",
                "scp_path",
            ],
        ),
    ];
    for (table, keys) in tables {
        let lines = spec_table_lines(table);
        assert!(!lines.is_empty(), "SPEC §9 has no `[{table}]` table");
        for key in keys {
            assert!(
                lines.iter().any(|l| is_key_line(l, key)),
                "SPEC §9 [{table}] has no `{key} = …` line (live or commented out)"
            );
        }
    }
}

/// SPEC §9 lists eight keys the code still hard-codes. A live line for any
/// of them would be a config file the loader rejects, so each may appear
/// only commented out, marked as not configurable yet.
#[test]
fn spec_constant_only_keys_are_commented_out_as_not_configurable() {
    let block = spec_section_9_toml();
    for key in [
        "floor_quantile",
        "floor_window_ms",
        "block_channels",
        "block_allowance_db",
        "mu_ratio_bounds",
        "char_gap_dits",
        "word_gap_dits",
        "cluster_alpha",
    ] {
        let lines: Vec<&str> = block.lines().filter(|l| l.contains(key)).collect();
        assert!(!lines.is_empty(), "SPEC §9 no longer lists `{key}`");
        for line in lines {
            assert!(
                line.trim_start().starts_with('#') && line.contains("not configurable yet"),
                "SPEC §9 mentions constant-only `{key}` outside a `# … not configurable \
                 yet` line: {line}"
            );
        }
    }
}

/// The node section must be one config file an operator can copy: the
/// receiver lives in `[input]` next to `[server]`, the command carries no
/// source flags, and the file actually loads. The Kiwi host is a marked
/// placeholder, and since a bare `<…>` is not a TOML integer, the port's
/// placeholder marker lives in its comment (the intent of
/// `readme_kiwi_commands_use_a_marked_placeholder_host` and
/// `readme_kiwi_commands_offer_a_marked_placeholder_port`, carried to TOML).
#[test]
fn readme_node_section_is_a_single_config_file() {
    let readme = doc("README.md");
    let node = section(&readme, "Run it as a node");
    let text = toml_block(node);
    let table: toml::Table = toml::from_str(&text)
        .unwrap_or_else(|e| panic!("README node config is not valid TOML: {e}\n{text}"));
    for t in ["server", "input"] {
        assert!(
            table.contains_key(t),
            "README node config has no [{t}] table"
        );
    }
    let host = table["input"]
        .get("host")
        .and_then(toml::Value::as_str)
        .expect("README node config's [input] has no string `host`");
    assert!(
        host.starts_with('<') && host.ends_with('>'),
        "README node config uses host `{host}`; it needs a `<placeholder>` the reader \
         must replace"
    );
    let port_line = text
        .lines()
        .find(|l| is_key_line(l, "port"))
        .expect("README node config's [input] has no `port` line");
    assert!(
        port_line.contains("<your-kiwi-port>"),
        "README node config's port line carries no `<your-kiwi-port>` marker: {port_line}"
    );

    let runs: Vec<String> = fenced_commands(node)
        .into_iter()
        .filter(|c| c.contains("manta run"))
        .collect();
    assert!(
        !runs.is_empty(),
        "README node section has no `manta run` command"
    );
    for cmd in &runs {
        assert!(
            cmd.contains("--config"),
            "README node command does not use the config file: {cmd}"
        );
        for flag in ["--kiwi-host", "--source", "--device"] {
            assert!(
                !cmd.split_whitespace().any(|t| t.starts_with(flag)),
                "README node command still selects the source with `{flag}`: {cmd}"
            );
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let wav = short_v1_wav(dir.path());
    let cfg = dir.path().join("manta.toml");
    fs::write(&cfg, &text).unwrap();
    let out = manta()
        .args(["decode", "--json", "--config"])
        .arg(&cfg)
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "README node config does not load (`manta decode --config`): {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The metrics endpoint (and every server) starts when the resolved config
/// has a `[server]` table -- from the file or `MANTA_SERVER_*` -- not
/// whenever `--config` is passed: a config without `[server]` runs with no
/// servers.
#[test]
fn architecture_section_8_does_not_tie_servers_to_the_flag() {
    let arch = doc("ARCHITECTURE.md");
    let s8 = squash_whitespace(section(&arch, "8. Configuration"));
    assert!(
        !s8.contains("whenever `--config` is set"),
        "ARCHITECTURE §8 still says the servers run whenever `--config` is set"
    );
    assert!(
        s8.contains("whenever the resolved config has a `[server]` table"),
        "ARCHITECTURE §8 no longer says when the servers run"
    );
}

/// MAN-76: the node section walks an operator through scaffolding and
/// checking the config file before `manta run`, as copyable commands.
#[test]
fn readme_node_section_mentions_config_init_and_check() {
    let readme = doc("README.md");
    let commands = fenced_commands(section(&readme, "Run it as a node"));
    for wanted in ["manta config init", "manta config check"] {
        assert!(
            commands.iter().any(|c| c.trim_start().starts_with(wanted)),
            "README node section has no `{wanted}` command: {commands:?}"
        );
    }
}

/// MAN-65 finding 2: CI cannot prove `manta.exe` starts on a machine without
/// the Visual C++ Redistributable, so the release-pipeline decision doc hands
/// that check to the release runbook by section name. The section must exist
/// and hold the actual procedure, not just a heading.
#[test]
fn release_runbook_has_the_clean_windows_check_the_decision_doc_cites() {
    const HEADING: &str =
        "Manual check: the Windows ZIP starts without the Visual C++ Redistributable";
    let decision = squash_whitespace(&doc(
        "docs/DECISIONS/2026-09-05-man65-release-pipeline-hardening.md",
    ));
    assert!(
        decision.contains(&format!("`docs/RUNBOOKS/release.md`, \"{HEADING}\"")),
        "the MAN-65 decision doc no longer cites the runbook's clean-Windows check by name"
    );
    let runbook = doc("docs/RUNBOOKS/release.md");
    let check = section(&runbook, HEADING);
    for needle in ["manta.exe --help", "vcruntime140.dll"] {
        assert!(
            check.contains(needle),
            "the runbook's clean-Windows check never says `{needle}`"
        );
    }
}

/// MAN-76: ARCHITECTURE §8's config bullet names the command that validates
/// a config without starting the daemon.
#[test]
fn architecture_section_8_mentions_config_check() {
    let arch = doc("ARCHITECTURE.md");
    let s8 = squash_whitespace(section(&arch, "8. Configuration"));
    assert!(
        s8.contains("`manta config check`"),
        "ARCHITECTURE §8 does not mention `manta config check`"
    );
    assert!(
        s8.contains("`manta config init`"),
        "ARCHITECTURE §8 does not mention `manta config init`"
    );
}

/// MAN-132: metrics has its own bind address (default 127.0.0.1). No
/// operator-facing doc may still say it shares `bind_addr` or that no
/// per-listener option exists.
#[test]
fn docs_describe_the_separate_metrics_bind_addr() {
    for rel in [
        "README.md",
        "ARCHITECTURE.md",
        "docs/RUNBOOKS/network-exposure.md",
        "docs/RUNBOOKS/node-health.md",
    ] {
        let text = squash_whitespace(&doc(rel));
        assert!(
            text.contains("metrics_bind_addr"),
            "{rel} never mentions metrics_bind_addr"
        );
        for stale in [
            "A per-listener bind option doesn't exist yet",
            "omit it and all three servers bind",
            "shares the same publicly-bound-by-default posture",
            "default bind `[server].bind_addr`",
        ] {
            assert!(!text.contains(stale), "{rel} still says {stale:?}");
        }
    }
}

/// MAN-116: the README and ARCHITECTURE name the sensitivity benchmark so a
/// reader can find how a sensitivity claim was produced.
#[test]
fn docs_name_the_sensitivity_benchmark() {
    assert!(doc("README.md").contains("manta bench sensitivity"));
    assert!(doc("ARCHITECTURE.md").contains("manta bench sensitivity"));
}

/// MAN-83: where `decoderVersion`'s format and the decoder-output versioning
/// rule are decided.
const MAN83_DECISION: &str =
    "docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md";

/// MAN-83: CHANGELOG.md is Keep a Changelog-shaped (Unreleased first) and
/// states the decoder-output rule downstream benchmark comparisons rely on,
/// linking the decision doc that makes it normative.
#[test]
fn changelog_opens_with_unreleased_and_states_the_decoder_output_rule() {
    let changelog = doc("CHANGELOG.md");
    assert_eq!(
        changelog.lines().find(|l| l.starts_with("## ")),
        Some("## [Unreleased]"),
        "CHANGELOG.md's first `## ` heading must be `## [Unreleased]`"
    );
    let text = squash_whitespace(&changelog);
    for needle in [
        "Keep a Changelog",
        "Semantic Versioning",
        "### Decoder output",
        "MINOR",
        "PATCH",
        MAN83_DECISION,
    ] {
        assert!(text.contains(needle), "CHANGELOG.md never says {needle:?}");
    }
    assert!(
        repo_root().join(MAN83_DECISION).is_file(),
        "CHANGELOG.md links {MAN83_DECISION}, which does not exist"
    );
}

/// MAN-83: the rule is only a promise if cutting a release applies it.
#[test]
fn release_runbook_applies_the_decoder_output_rule() {
    let runbook = doc("docs/RUNBOOKS/release.md");
    let cutting = squash_whitespace(section(&runbook, "Cutting a release"));
    for needle in ["CHANGELOG.md", "### Decoder output", "MINOR"] {
        assert!(
            cutting.contains(needle),
            "release runbook's \"Cutting a release\" never says {needle:?}"
        );
    }
}

/// MAN-83: ARCHITECTURE §7 names `decoderVersion`'s build-identity form and
/// points at its decision doc.
#[test]
fn architecture_section_7_describes_decoder_version() {
    let arch = doc("ARCHITECTURE.md");
    let s7 = squash_whitespace(section(&arch, "7. Output layer"));
    for needle in ["decoderVersion", "manta-<version>+<commit>", MAN83_DECISION] {
        assert!(s7.contains(needle), "ARCHITECTURE §7 never says {needle:?}");
    }
}

// ---- MAN-124: the log's level and format controls, documented and real ----

/// MAN-124: README's `## Logs` documents `RUST_LOG` as the underlying
/// mechanism, the four flags and `NO_COLOR`.
#[test]
fn readme_logs_section_documents_rust_log_and_the_flags() {
    let readme = doc("README.md");
    let logs = squash_whitespace(section(&readme, "Logs"));
    for needle in [
        "RUST_LOG",
        "-v",
        "-q",
        "--log-level",
        "--log-format json",
        "NO_COLOR",
    ] {
        assert!(
            logs.contains(needle),
            "README `## Logs` never says {needle:?}"
        );
    }
}

/// MAN-124: the flags README's `## Logs` names exist on `manta run`.
#[test]
fn readme_logs_flags_exist_in_run_help() {
    let out = manta().args(["run", "--help"]).output().unwrap();
    assert!(out.status.success(), "manta run --help: {:?}", out.status);
    let help = String::from_utf8(out.stdout).unwrap();
    for flag in ["--verbose", "--quiet", "--log-level", "--log-format"] {
        assert!(help.contains(flag), "`manta run --help` has no {flag}");
    }
}

/// MAN-124: the packaging guide says how to change the log level and format
/// under each service manager.
#[test]
fn packaging_readme_says_how_to_change_log_level_and_format() {
    let guide = doc("packaging/README.md");
    let systemd = squash_whitespace(section(&guide, "Linux: systemd"));
    for needle in ["--log-format json", "RUST_LOG"] {
        assert!(
            systemd.contains(needle),
            "systemd section never says {needle:?}"
        );
    }
    let launchd = squash_whitespace(section(&guide, "macOS: LaunchDaemon"));
    assert!(
        launchd.contains("--log-format json"),
        "launchd section never says --log-format json"
    );
    let docker = squash_whitespace(section(&guide, "Docker Compose"));
    assert!(
        docker.contains("--log-format"),
        "Docker section never says --log-format"
    );
}

/// MAN-124: ARCHITECTURE.md no longer calls the log plain `fmt` output.
#[test]
fn architecture_logging_bullet_is_current() {
    let arch = squash_whitespace(&doc("ARCHITECTURE.md"));
    assert!(
        !arch.contains("plain `fmt` output"),
        "ARCHITECTURE.md still says plain `fmt` output"
    );
    assert!(
        arch.contains("--log-format json"),
        "ARCHITECTURE.md never says --log-format json"
    );
}
