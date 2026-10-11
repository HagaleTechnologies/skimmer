//! MAN-116: `manta bench sensitivity` end to end through the binary, on a
//! CI-sized grid (one AWGN series, two SNRs, 30 s). Harness correctness only:
//! the command is a measuring instrument, not a decoder gate.

use std::process::{Command, Output};

const SMALL: &[&str] = &[
    "bench",
    "sensitivity",
    "--conditions",
    "awgn",
    "--wpm",
    "25",
    "--snr-db",
    "-10,20",
    "--duration-s",
    "30",
];

fn manta(args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(args).output().unwrap()
}

/// The `recall` column's numerator, e.g. 10 from "10/10".
fn recall_of(row: &str) -> u32 {
    let cols: Vec<&str> = row.split_whitespace().collect();
    // "AWGN", "25", "WPM", "20", "dB", "<recall>", ...
    cols[5].split('/').next().unwrap().parse().unwrap()
}

#[test]
fn small_sweep_prints_a_table_and_progress() {
    let out = manta(SMALL);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stdout.starts_with("manta "), "{stdout}");
    assert!(
        stdout.contains("condition        speed     SNR  recall  bogus   CER  CER<0.10  spot SNR"),
        "{stdout}"
    );
    let low = stdout
        .lines()
        .find(|l| l.starts_with("AWGN") && l.contains("-10 dB"))
        .unwrap();
    // The no-signal point is reported, not an error.
    assert!(low.contains("0/10") && low.contains("1.00"), "{low}");
    let high = stdout
        .lines()
        .find(|l| l.starts_with("AWGN") && l.contains(" 20 dB"))
        .unwrap();
    // Harness anti-vacuity floor, not a decoder gate: measured 10/10 in planning.
    assert!(recall_of(high) >= 5, "{high}");
    assert!(stdout.contains(
        "Regenerate: manta bench sensitivity --conditions awgn --wpm 25 --snr-db -10,20 --duration-s 30"
    ));
    assert!(
        stderr.contains("[1/2] AWGN, 25 WPM, -10 dB") && stderr.contains("done in"),
        "{stderr}"
    );
    // Progress never reaches stdout.
    assert!(!stdout.contains("done in") && !stdout.contains("[1/2]"));
}

#[test]
fn bad_values_exit_2_with_nothing_on_stdout() {
    for args in [
        &["bench", "sensitivity", "--wpm", "0"][..],
        &["bench", "sensitivity", "--json", "--markdown"][..],
    ] {
        let out = manta(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    let out = manta(&["bench", "sensitivity", "--wpm", "0"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("0 is outside 5 to 60 WPM"));
}

#[test]
fn bench_help_lists_sensitivity() {
    let out = manta(&["bench", "--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("sensitivity"));
    let out = manta(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        help.lines().any(|l| l.trim_start().starts_with("bench ")),
        "{help}"
    );
}

#[test]
fn json_output_is_byte_identical_for_any_job_count_and_parses() {
    let run = |jobs: &str| {
        let mut args = SMALL.to_vec();
        args.extend(["--json", "--jobs", jobs]);
        manta(&args)
    };
    let (a, b) = (run("1"), run("2"));
    assert!(
        a.status.success() && b.status.success(),
        "{}",
        String::from_utf8_lossy(&a.stderr)
    );
    assert_eq!(a.stdout, b.stdout);
    let v: serde_json::Value = serde_json::from_slice(&a.stdout).unwrap();
    assert_eq!(v["snr_ref_hz"], 500);
    assert_eq!(v["points"].as_array().unwrap().len(), 2);
    assert_eq!(v["points"][0]["spotted"], 0);
    assert_eq!(v["points"][0]["cer_mean"], 1.0);
    assert!(v["points"][1]["bogus_calls"].as_array().unwrap().is_empty());
    assert_eq!(
        v["command"],
        "manta bench sensitivity --conditions awgn --wpm 25 --snr-db -10,20 --duration-s 30 --json"
    );
}

#[test]
fn markdown_output_is_a_table() {
    let mut args = SMALL.to_vec();
    args.push("--markdown");
    let out = manta(&args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let md = String::from_utf8(out.stdout).unwrap();
    assert!(
        md.lines()
            .any(|l| l.starts_with("| Condition | Speed | SNR |")),
        "{md}"
    );
    assert_eq!(md.lines().filter(|l| l.starts_with("| AWGN |")).count(), 2);
    assert!(md.contains(
        "Regenerate: `manta bench sensitivity --conditions awgn --wpm 25 --snr-db -10,20 --duration-s 30 --markdown`"
    ));
}
