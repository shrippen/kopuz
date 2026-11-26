//! Buttplug protocol v3: the messages the plugin sends and reads. Every
//! packet is a JSON array of messages, each with an `Id`.

use serde_json::{Value, json};

pub const MESSAGE_VERSION: u32 = 3;
const CLIENT_NAME: &str = "kopuz";
/// The actuator types a plain 0 to 1 scalar drives; a stroker's `Position`
/// is a place, not a strength, and is left alone.
const DRIVEN: [&str; 5] = ["Vibrate", "Oscillate", "Rotate", "Constrict", "Inflate"];
/// Pause between commands to a device when it names none.
const DEFAULT_GAP_MS: u32 = 100;

/// The `Id` of the opening handshake; later messages count up from it.
pub const HANDSHAKE_ID: u32 = 1;

fn packet(message: Value) -> String {
    Value::Array(vec![message]).to_string()
}

pub fn handshake(id: u32) -> String {
    packet(json!({ "RequestServerInfo": {
        "Id": id, "ClientName": CLIENT_NAME, "MessageVersion": MESSAGE_VERSION,
    }}))
}

pub fn ping(id: u32) -> String {
    packet(json!({ "Ping": { "Id": id } }))
}

pub fn start_scanning(id: u32) -> String {
    packet(json!({ "StartScanning": { "Id": id } }))
}

pub fn request_device_list(id: u32) -> String {
    packet(json!({ "RequestDeviceList": { "Id": id } }))
}

pub fn stop_all(id: u32) -> String {
    packet(json!({ "StopAllDevices": { "Id": id } }))
}

/// One actuator's new value, 0 to 1.
pub struct Scalar {
    pub index: u32,
    pub kind: String,
    pub value: f64,
}

pub fn scalar_cmd(id: u32, device: u32, scalars: &[Scalar]) -> String {
    let scalars: Vec<Value> = scalars
        .iter()
        .map(|scalar| {
            json!({
                "Index": scalar.index,
                "Scalar": (scalar.value * 10_000.0).round() / 10_000.0,
                "ActuatorType": scalar.kind,
            })
        })
        .collect();
    packet(json!({ "ScalarCmd": { "Id": id, "DeviceIndex": device, "Scalars": scalars } }))
}

/// An actuator of a device that a scalar drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actuator {
    /// Its place in the device's `ScalarCmd` list, which a command names.
    pub index: u32,
    pub kind: String,
    pub steps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub index: u32,
    pub name: String,
    pub actuators: Vec<Actuator>,
    /// The least time between two commands to it.
    pub timing_gap_ms: u32,
}

impl Device {
    /// Whether any of its actuators can be driven; a stroker cannot.
    pub fn supported(&self) -> bool {
        !self.actuators.is_empty()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    ServerInfo { max_ping_ms: u32 },
    DeviceList(Vec<Device>),
    DeviceAdded(Device),
    DeviceRemoved(u32),
    Error(String),
}

fn device(value: &Value) -> Option<Device> {
    let index = value.get("DeviceIndex")?.as_u64()? as u32;
    let scalars = value.pointer("/DeviceMessages/ScalarCmd");
    let actuators = scalars
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, actuator)| {
            DRIVEN.contains(&actuator["ActuatorType"].as_str().unwrap_or_default())
        })
        .map(|(at, actuator)| Actuator {
            index: at as u32,
            kind: actuator["ActuatorType"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            steps: actuator["StepCount"].as_u64().unwrap_or(1).max(1) as u32,
        })
        .collect();
    let name = value["DeviceName"].as_str().unwrap_or_default().to_string();
    let timing_gap_ms = value["DeviceMessageTimingGap"]
        .as_u64()
        .map_or(DEFAULT_GAP_MS, |gap| gap as u32);
    Some(Device {
        index,
        name,
        actuators,
        timing_gap_ms,
    })
}

fn message(kind: &str, body: &Value) -> Option<Incoming> {
    match kind {
        "ServerInfo" => Some(Incoming::ServerInfo {
            max_ping_ms: body["MaxPingTime"].as_u64().unwrap_or(0) as u32,
        }),
        "DeviceList" => {
            let devices = body["Devices"].as_array()?;
            Some(Incoming::DeviceList(
                devices.iter().filter_map(device).collect(),
            ))
        }
        "DeviceAdded" => device(body).map(Incoming::DeviceAdded),
        "DeviceRemoved" => body["DeviceIndex"]
            .as_u64()
            .map(|index| Incoming::DeviceRemoved(index as u32)),
        "Error" => Some(Incoming::Error(
            body["ErrorMessage"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
        )),
        _ => None,
    }
}

/// The messages of a packet this plugin cares about; anything else is skipped.
pub fn parse(text: &str) -> Vec<Incoming> {
    let Ok(Value::Array(messages)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    messages
        .iter()
        .filter_map(|entry| entry.as_object()?.iter().next())
        .filter_map(|(kind, body)| message(kind, body))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_names_the_client_and_version() {
        let sent: Value = serde_json::from_str(&handshake(1)).unwrap();
        let body = &sent[0]["RequestServerInfo"];
        assert_eq!(body["Id"], 1);
        assert_eq!(body["ClientName"], "kopuz");
        assert_eq!(body["MessageVersion"], 3);
    }

    #[test]
    fn a_scalar_command_names_device_actuator_and_type() {
        let scalars = [Scalar {
            index: 1,
            kind: "Vibrate".into(),
            value: 0.30000000000000004,
        }];
        let sent: Value = serde_json::from_str(&scalar_cmd(7, 2, &scalars)).unwrap();
        let body = &sent[0]["ScalarCmd"];
        assert_eq!(body["Id"], 7);
        assert_eq!(body["DeviceIndex"], 2);
        assert_eq!(body["Scalars"][0]["Index"], 1);
        assert_eq!(body["Scalars"][0]["Scalar"], 0.3);
        assert_eq!(body["Scalars"][0]["ActuatorType"], "Vibrate");
    }

    #[test]
    fn server_info_carries_the_ping_time() {
        let text =
            r#"[{"ServerInfo":{"Id":1,"MessageVersion":3,"MaxPingTime":500,"ServerName":"x"}}]"#;
        assert_eq!(parse(text), vec![Incoming::ServerInfo { max_ping_ms: 500 }]);
    }

    const TOY: &str = r#"{"DeviceName":"Toy","DeviceIndex":4,"DeviceMessages":{"ScalarCmd":[
        {"StepCount":20,"FeatureDescriptor":"a","ActuatorType":"Position"},
        {"StepCount":10,"FeatureDescriptor":"b","ActuatorType":"Vibrate"}]}}"#;

    #[test]
    fn a_device_keeps_its_vibrators_at_their_list_index() {
        let expected = Device {
            index: 4,
            name: "Toy".into(),
            actuators: vec![Actuator {
                index: 1,
                kind: "Vibrate".into(),
                steps: 10,
            }],
            timing_gap_ms: DEFAULT_GAP_MS,
        };
        let list = format!(r#"[{{"DeviceList":{{"Id":2,"Devices":[{TOY}]}}}}]"#);
        assert_eq!(
            parse(&list),
            vec![Incoming::DeviceList(vec![expected.clone()])]
        );

        let added = format!(
            r#"[{{"DeviceAdded":{}}}]"#,
            TOY.replacen('{', r#"{"Id":0,"#, 1)
        );
        assert_eq!(parse(&added), vec![Incoming::DeviceAdded(expected)]);
    }

    #[test]
    fn removal_and_errors_parse_and_the_rest_is_skipped() {
        let text = r#"[{"DeviceRemoved":{"Id":0,"DeviceIndex":4}},{"Ok":{"Id":3}},
            {"Error":{"Id":0,"ErrorCode":2,"ErrorMessage":"bad"}}]"#;
        assert_eq!(
            parse(text),
            vec![Incoming::DeviceRemoved(4), Incoming::Error("bad".into())]
        );
        assert!(parse("not json").is_empty());
    }

    const TWO_MOTOR: &str = r#"[{"DeviceAdded":{"DeviceIndex":3,"DeviceMessageTimingGap":100,"DeviceMessages":{"ScalarCmd":[{"ActuatorType":"Vibrate","FeatureDescriptor":"Motor 1","StepCount":100},{"ActuatorType":"Vibrate","FeatureDescriptor":"Motor 2","StepCount":100}],"StopDeviceCmd":{}},"DeviceName":"Simulated 2-Motor Vibrator","Id":0}}]"#;
    const STROKER: &str = r#"[{"DeviceAdded":{"DeviceIndex":1,"DeviceMessageTimingGap":100,"DeviceMessages":{"LinearCmd":[{"ActuatorType":"Position","FeatureDescriptor":"Linear Axis","StepCount":1000}],"ScalarCmd":[{"ActuatorType":"Position","FeatureDescriptor":"Linear Axis","StepCount":1000}],"StopDeviceCmd":{}},"DeviceName":"Simulated Stroker","Id":0}}]"#;

    #[test]
    fn a_two_motor_device_gets_both_actuators_and_its_gap() {
        let [Incoming::DeviceAdded(device)] = &parse(TWO_MOTOR)[..] else {
            panic!("one added device");
        };
        assert_eq!(device.name, "Simulated 2-Motor Vibrator");
        assert_eq!(device.timing_gap_ms, 100);
        assert_eq!(device.actuators.len(), 2);
        assert_eq!(device.actuators[1].index, 1);
        assert_eq!(device.actuators[1].steps, 100);
        assert!(device.supported());
    }

    #[test]
    fn a_stroker_is_unsupported() {
        let [Incoming::DeviceAdded(device)] = &parse(STROKER)[..] else {
            panic!("one added device");
        };
        assert!(!device.supported());
    }

    #[test]
    fn other_scalar_types_are_driven_and_a_missing_gap_defaults() {
        let text = r#"[{"DeviceAdded":{"DeviceIndex":2,"DeviceName":"O","DeviceMessages":{"ScalarCmd":[{"ActuatorType":"Oscillate","StepCount":5}]}}}]"#;
        let [Incoming::DeviceAdded(device)] = &parse(text)[..] else {
            panic!("one added device");
        };
        assert_eq!(device.actuators[0].kind, "Oscillate");
        assert_eq!(device.timing_gap_ms, DEFAULT_GAP_MS);
    }
}
