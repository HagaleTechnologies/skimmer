//! MAN-131 scenario 1 against the real `manta` binary: a live audio input
//! that cannot be found is reported with what was requested, the 48000 Hz
//! requirement and the command that lists the selectable inputs, by every
//! command that opens one.
//!
//! Safe on both CI runners: enumerating devices needs no microphone
//! permission (`source_check.rs`'s `devices_json_smoke…` already does it),
//! and a name that matches nothing never opens a stream.

use std::process::Command;

/// The binary with every `MANTA_*` variable removed, as in `cli.rs`.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

#[test]
fn a_missing_audio_input_names_itself_the_rate_and_manta_devices_in_every_command() {
    let name = format!("manta-man131-no-such-input-{}", std::process::id());
    let name = name.as_str();
    for args in [
        vec!["run", "--device", name, "--dial-freq-hz", "14030000"],
        vec!["listen", "--device", name, "--dial-freq-hz", "14030000"],
        vec![
            "soak",
            "--device",
            name,
            "--dial-freq-hz",
            "14030000",
            "--duration",
            "3",
        ],
        vec![
            "doctor",
            "--device",
            name,
            "--dial-freq-hz",
            "14030000",
            "--duration",
            "3",
        ],
        vec!["check", "--device", name],
    ] {
        let out = manta().args(&args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        let want = format!(
            "no audio input matching \"{name}\" (manta needs a 48000 Hz input) -- \
             run `manta devices` to list the inputs --device can select"
        );
        assert!(
            stderr.contains(&want),
            "{args:?}: {want:?} missing from {stderr}"
        );
    }
}
