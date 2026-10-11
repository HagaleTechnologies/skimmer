//! Owned device identities for discovery without opening a capture stream.

/// A selectable input identity. The backend's rate maxima include outputs,
/// so are deliberately omitted: enumeration cannot promise a capture rate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AudioDevice {
    pub name: String,
    pub input_channels: u16,
}

/// Complete backend arguments, including serial/device discrimination.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SoapyDevice {
    pub args: String,
}

/// Enumeration failures are retained alongside successful backend results.
#[derive(Debug)]
pub enum Enumeration<T> {
    Ok(Vec<T>),
    Error(String),
    Disabled,
}

#[derive(Debug)]
pub struct DeviceInventory {
    pub audio: Enumeration<AudioDevice>,
    pub soapy: Enumeration<SoapyDevice>,
}

/// Discover identities only; this does not open a sample stream.
pub fn enumerate() -> DeviceInventory {
    let audio = coppa_audio::list_devices().map_err(|error| error.to_string());
    #[cfg(feature = "soapy")]
    let soapy = Some(
        soapysdr::enumerate("")
            .map(soapy_records)
            .map_err(|error| error.to_string()),
    );
    #[cfg(not(feature = "soapy"))]
    let soapy = None;
    assemble(audio, soapy)
}

#[cfg(feature = "soapy")]
fn soapy_records(args: Vec<soapysdr::Args>) -> Vec<String> {
    args.into_iter().map(|args| args.to_string()).collect()
}

fn assemble(
    audio: Result<Vec<coppa_audio::AudioDevice>, String>,
    soapy: Option<Result<Vec<String>, String>>,
) -> DeviceInventory {
    let audio = match audio {
        Ok(devices) => {
            let mut records: Vec<_> = devices
                .into_iter()
                .filter(|device| device.input_channels > 0)
                .map(|device| AudioDevice {
                    name: device.name,
                    input_channels: device.input_channels,
                })
                .collect();
            records.sort_by(|a, b| (&a.name, a.input_channels).cmp(&(&b.name, b.input_channels)));
            Enumeration::Ok(records)
        }
        Err(error) => Enumeration::Error(error),
    };
    let soapy = match soapy {
        None => Enumeration::Disabled,
        Some(Err(error)) => Enumeration::Error(error),
        Some(Ok(mut selectors)) => {
            selectors.sort();
            Enumeration::Ok(
                selectors
                    .into_iter()
                    .map(|args| SoapyDevice { args })
                    .collect(),
            )
        }
    };
    DeviceInventory { audio, soapy }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(name: &str, inputs: u16, outputs: u16) -> coppa_audio::AudioDevice {
        coppa_audio::AudioDevice {
            name: name.into(),
            input_channels: inputs,
            output_channels: outputs,
            max_sample_rate: u32::MAX,
        }
    }

    #[test]
    fn input_filter_preserves_names_channels_duplicates_and_order() {
        let inventory = assemble(
            Ok(vec![
                audio("z\nUSB", 2, 2),
                audio("Speaker", 0, 2),
                audio("Mic", 1, 0),
                audio("Mic", 2, 0),
            ]),
            Some(Ok(vec![
                "driver=rtl,serial=2".into(),
                "driver=rtl,serial=1".into(),
            ])),
        );
        let Enumeration::Ok(inputs) = inventory.audio else {
            panic!("expected inputs")
        };
        assert_eq!(
            inputs,
            vec![
                AudioDevice {
                    name: "Mic".into(),
                    input_channels: 1
                },
                AudioDevice {
                    name: "Mic".into(),
                    input_channels: 2
                },
                AudioDevice {
                    name: "z\nUSB".into(),
                    input_channels: 2
                },
            ]
        );
        let Enumeration::Ok(soapy) = inventory.soapy else {
            panic!("expected SDRs")
        };
        assert_eq!(
            soapy.iter().map(|d| d.args.as_str()).collect::<Vec<_>>(),
            ["driver=rtl,serial=1", "driver=rtl,serial=2"]
        );
    }

    #[test]
    fn backend_errors_preserve_other_backend_success() {
        let inventory = assemble(
            Err("audio denied".into()),
            Some(Ok(vec!["driver=x,serial=y".into()])),
        );
        assert!(matches!(inventory.audio, Enumeration::Error(ref e) if e == "audio denied"));
        assert!(
            matches!(inventory.soapy, Enumeration::Ok(ref devices) if devices[0].args == "driver=x,serial=y")
        );
        let inventory = assemble(Ok(vec![audio("Mic", 1, 0)]), Some(Err("SDR denied".into())));
        assert!(matches!(inventory.audio, Enumeration::Ok(ref devices) if devices.len() == 1));
        assert!(matches!(inventory.soapy, Enumeration::Error(ref e) if e == "SDR denied"));
    }

    #[test]
    fn disabled_is_distinct_from_successful_empty_discovery() {
        let disabled = assemble(Ok(vec![]), None);
        assert!(matches!(disabled.audio, Enumeration::Ok(ref devices) if devices.is_empty()));
        assert!(matches!(disabled.soapy, Enumeration::Disabled));
        let empty = assemble(Ok(vec![]), Some(Ok(vec![])));
        assert!(matches!(empty.soapy, Enumeration::Ok(ref devices) if devices.is_empty()));
    }

    #[cfg(feature = "soapy")]
    #[test]
    fn soapy_conversion_preserves_every_backend_argument() {
        let args: soapysdr::Args = "driver=rtlsdr,serial=00000001,label=Receiver A".into();
        let records = soapy_records(vec![args]);
        let round_trip: soapysdr::Args = records[0].as_str().into();
        assert_eq!(round_trip.get("driver"), Some("rtlsdr"));
        assert_eq!(round_trip.get("serial"), Some("00000001"));
        assert_eq!(round_trip.get("label"), Some("Receiver A"));
    }
}
