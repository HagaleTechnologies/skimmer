//! Presentation and exit semantics for device discovery.

use std::io::{self, Write};

use manta_input::devices::{AudioDevice, DeviceInventory, Enumeration, SoapyDevice};
use serde::Serialize;

/// Write one complete inventory, then return the discovery exit status.
pub fn run(json: bool) -> anyhow::Result<i32> {
    let report = DeviceReport::from(manta_input::devices::enumerate());
    let mut stdout = io::stdout().lock();
    if json {
        serde_json::to_writer(&mut stdout, &report)?;
        writeln!(stdout)?;
    } else {
        write!(stdout, "{}", report.text())?;
    }
    stdout.flush()?;
    write!(io::stderr().lock(), "{}", report.errors())?;
    Ok(report.exit_code())
}

#[derive(Serialize)]
struct Section<T> {
    status: &'static str,
    devices: Vec<T>,
    error: Option<String>,
}

impl<T> From<Enumeration<T>> for Section<T> {
    fn from(result: Enumeration<T>) -> Self {
        match result {
            Enumeration::Ok(devices) => Self {
                status: "ok",
                devices,
                error: None,
            },
            Enumeration::Error(error) => Self {
                status: "error",
                devices: vec![],
                error: Some(error),
            },
            Enumeration::Disabled => Self {
                status: "disabled",
                devices: vec![],
                error: None,
            },
        }
    }
}

#[derive(Serialize)]
struct DeviceReport {
    audio: Section<AudioDevice>,
    soapy: Section<SoapyDevice>,
    hpsdr: Section<()>,
    kiwi: Section<()>,
}

impl From<DeviceInventory> for DeviceReport {
    fn from(inventory: DeviceInventory) -> Self {
        Self {
            audio: inventory.audio.into(),
            soapy: inventory.soapy.into(),
            hpsdr: Section {
                status: "not_supported",
                devices: vec![],
                error: None,
            },
            kiwi: Section {
                status: "explicit_host",
                devices: vec![],
                error: None,
            },
        }
    }
}

impl DeviceReport {
    fn exit_code(&self) -> i32 {
        i32::from(self.audio.error.is_some() || self.soapy.error.is_some())
    }

    fn errors(&self) -> String {
        let mut output = String::new();
        for (name, error) in [
            ("audio", &self.audio.error),
            ("SoapySDR", &self.soapy.error),
        ] {
            if let Some(error) = error {
                // Backend error strings are untrusted terminal text too.
                output.push_str(&format!(
                    "error: {name} device enumeration failed: {}\n",
                    escaped_error(error)
                ));
            }
        }
        output
    }

    fn text(&self) -> String {
        let mut output = String::new();
        if self.audio.error.is_some() {
            output.push_str("Audio inputs: enumeration failed.\n");
        } else if self.audio.devices.is_empty() {
            output.push_str("Audio inputs: none found.\n");
        } else {
            output.push_str("Audio inputs:\n");
            for device in &self.audio.devices {
                let plural = if device.input_channels == 1 { "" } else { "s" };
                output.push_str(&format!(
                    "  {} ({} input channel{plural})\n",
                    quoted(&device.name),
                    device.input_channels
                ));
            }
            output.push_str("Select an audio input with --device NAME; names match case-insensitively by substring.\n");
        }
        if self.soapy.status == "disabled" {
            output.push_str("SoapySDR: unavailable in this build; build with --features soapy.\n");
        } else if self.soapy.error.is_some() {
            output.push_str("SoapySDR devices: enumeration failed.\n");
        } else if self.soapy.devices.is_empty() {
            output.push_str("SoapySDR devices: none found.\n");
        } else {
            output.push_str("SoapySDR devices:\n");
            for device in &self.soapy.devices {
                output.push_str(&format!("  {}\n", quoted(&device.args)));
            }
            output.push_str("Select a SoapySDR device with --soapy-driver ARGS.\n");
        }
        output.push_str("HPSDR: automatic discovery is not supported; use --hpsdr-host HOST with an hpsdr-enabled build.\nKiwiSDR: use --kiwi-host HOST.\n");
        output
    }
}

fn quoted(value: &str) -> String {
    // JSON escaping quotes C0 characters (including ANSI ESC) and names.
    serde_json::to_string(value)
        .expect("serializing a string cannot fail")
        .chars()
        .map(|c| {
            if c.is_control() {
                format!("\\u{:04x}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn escaped_error(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_input::devices::{AudioDevice, SoapyDevice};

    #[test]
    fn empty_disabled_and_host_guidance_have_distinct_statuses() {
        let inventory = DeviceInventory {
            audio: Enumeration::Ok(vec![]),
            soapy: Enumeration::Disabled,
        };
        let report = DeviceReport::from(inventory);
        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            report.text(),
            "Audio inputs: none found.\nSoapySDR: unavailable in this build; build with --features soapy.\nHPSDR: automatic discovery is not supported; use --hpsdr-host HOST with an hpsdr-enabled build.\nKiwiSDR: use --kiwi-host HOST.\n"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["audio"]["status"], "ok");
        assert_eq!(json["audio"]["devices"], serde_json::json!([]));
        assert!(json["audio"]["error"].is_null());
        assert_eq!(json["soapy"]["status"], "disabled");
        assert_eq!(json["hpsdr"]["status"], "not_supported");
        assert_eq!(json["kiwi"]["status"], "explicit_host");
    }

    #[test]
    fn names_are_escaped_and_selectors_preserve_identity_in_both_formats() {
        let report = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Ok(vec![AudioDevice {
                name: "USB\n\u{1b}[31m\"".into(),
                input_channels: 2,
            }]),
            soapy: Enumeration::Ok(vec![SoapyDevice {
                args: "driver=rtl, serial=00001".into(),
            }]),
        });
        let text = report.text();
        assert!(text.contains("  \"USB\\n\\u001b[31m\\\"\" (2 input channels)"));
        assert!(!text.contains('\u{1b}'));
        assert_eq!(quoted("\u{9b}31m"), "\"\\u009b31m\"");
        assert!(text.contains("  \"driver=rtl, serial=00001\""));
        assert!(text.contains("--device NAME"));
        assert!(text.contains("--soapy-driver ARGS"));
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["audio"]["devices"][0]["name"], "USB\n\u{1b}[31m\"");
        assert_eq!(
            json["soapy"]["devices"][0]["args"],
            "driver=rtl, serial=00001"
        );
        assert!(json["audio"]["devices"][0].get("max_sample_rate").is_none());
    }

    #[test]
    fn error_report_retains_partial_success_and_returns_failure() {
        let report = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Error("permission denied".into()),
            soapy: Enumeration::Ok(vec![SoapyDevice {
                args: "driver=rtl,serial=1".into(),
            }]),
        });
        assert_eq!(report.exit_code(), 1);
        assert!(report
            .text()
            .starts_with("Audio inputs: enumeration failed.\nSoapySDR devices:\n"));
        assert_eq!(
            report.errors(),
            "error: audio device enumeration failed: permission denied\n"
        );
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["audio"]["status"], "error");
        assert_eq!(json["audio"]["error"], "permission denied");
        assert_eq!(json["audio"]["devices"], serde_json::json!([]));
        assert_eq!(json["soapy"]["devices"][0]["args"], "driver=rtl,serial=1");
    }

    #[test]
    fn empty_soapy_is_success_and_multiple_errors_are_reported() {
        let empty = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Ok(vec![]),
            soapy: Enumeration::Ok(vec![]),
        });
        assert!(empty.text().contains("SoapySDR devices: none found."));
        assert_eq!(empty.exit_code(), 0);
        let failed = DeviceReport::from(DeviceInventory {
            audio: Enumeration::Error("A".into()),
            soapy: Enumeration::Error("B".into()),
        });
        assert_eq!(
            failed.errors(),
            "error: audio device enumeration failed: A\nerror: SoapySDR device enumeration failed: B\n"
        );
        assert_eq!(failed.exit_code(), 1);
    }
}
