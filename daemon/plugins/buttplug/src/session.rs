//! One live Buttplug connection: handshake, the device list, and commands
//! that only go out when a device's step changed.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::follow::quantize;
use crate::protocol::{self, Device, Incoming, Scalar};
use crate::ws::{Endpoint, Link, WebSocket};

/// How long each wait for the server's `ServerInfo` is, and how many.
const INFO_WAIT: Duration = Duration::from_secs(1);
const INFO_TRIES: u32 = 5;
/// A drain waits this long for more; a quiet socket costs one wait per call.
const DRAIN_WAIT: Duration = Duration::from_millis(1);
const MAX_DRAIN: u32 = 32;
/// The audio calls come about every 50 ms; a ping goes out three times per
/// `MaxPingTime`.
const CALL_MS: u32 = 50;
const PINGS_PER_WINDOW: u32 = 3;

/// The key of a device that names itself nothing.
const DEFAULT_NAME: &str = "Device";

/// What a panel can show: whether Intiface is connected, its devices and
/// what was last sent to them.
#[derive(Debug, Default, Clone)]
pub struct DeviceState {
    pub connected: bool,
    pub devices: BTreeMap<u32, Device>,
    /// The step last sent per (device, actuator); absent means 0.
    pub sent: BTreeMap<(u32, u32), u32>,
    /// When a device was last sent a command, for its timing gap.
    pub sent_at: BTreeMap<u32, Instant>,
}

impl DeviceState {
    fn sent_step(&self, device: u32, actuator: u32) -> u32 {
        self.sent.get(&(device, actuator)).copied().unwrap_or(0)
    }

    /// Each device's section key in the panel: its name, or `name#index` when
    /// two devices share a name.
    pub fn keys(&self) -> BTreeMap<u32, String> {
        let name_of = |device: &Device| match device.name.is_empty() {
            true => DEFAULT_NAME.to_string(),
            false => device.name.clone(),
        };
        self.devices
            .values()
            .map(|device| {
                let name = name_of(device);
                let twins = self.devices.values().filter(|d| name_of(d) == name).count();
                match twins > 1 {
                    true => (device.index, format!("{name}#{}", device.index)),
                    false => (device.index, name),
                }
            })
            .collect()
    }

    pub fn index_of(&self, key: &str) -> Option<u32> {
        self.keys()
            .into_iter()
            .find_map(|(index, name)| (name == key).then_some(index))
    }

    /// The last value sent to a device, from 0 to 1: its strongest actuator.
    pub fn now(&self, device: &Device) -> f32 {
        device
            .actuators
            .iter()
            .map(|actuator| {
                self.sent_step(device.index, actuator.index) as f32 / actuator.steps as f32
            })
            .fold(0.0, f32::max)
    }

    /// Whether the device's timing gap has passed since its last command.
    fn gap_passed(&self, device: &Device, now: Instant) -> bool {
        let gap = Duration::from_millis(device.timing_gap_ms.into());
        self.sent_at
            .get(&device.index)
            .is_none_or(|at| now.duration_since(*at) >= gap)
    }
}

pub struct Session<L: Link> {
    socket: WebSocket<L>,
    next_id: u32,
    pub state: DeviceState,
    /// Calls between pings; `None` when the server wants none.
    ping_every: Option<u32>,
    calls: u32,
}

impl<L: Link> Session<L> {
    /// Upgrades `link`, shakes hands and asks for devices.
    pub fn open(link: L, endpoint: &Endpoint, seed: u64) -> Result<Self, String> {
        let mut socket = WebSocket::connect(link, endpoint, seed).map_err(|e| e.to_string())?;
        socket
            .send_text(&protocol::handshake(protocol::HANDSHAKE_ID))
            .map_err(|e| e.to_string())?;
        let max_ping_ms = wait_for_info(&mut socket)?;

        let mut session = Session {
            socket,
            next_id: protocol::HANDSHAKE_ID + 1,
            state: DeviceState {
                connected: true,
                ..DeviceState::default()
            },
            ping_every: (max_ping_ms > 0)
                .then(|| (max_ping_ms / PINGS_PER_WINDOW / CALL_MS).max(1)),
            calls: 0,
        };
        let scan = protocol::start_scanning(session.take_id());
        session.send(&scan)?;
        let list = protocol::request_device_list(session.take_id());
        session.send(&list)?;
        Ok(session)
    }

    fn take_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id - 1
    }

    fn send(&mut self, text: &str) -> Result<(), String> {
        self.socket.send_text(text).map_err(|e| e.to_string())
    }

    /// Reads what the server sent, so its buffer never fills.
    fn drain(&mut self) -> Result<(), String> {
        for _ in 0..MAX_DRAIN {
            let Some(text) = self
                .socket
                .next_text(DRAIN_WAIT)
                .map_err(|e| e.to_string())?
            else {
                return Ok(());
            };
            for message in protocol::parse(&text) {
                self.apply(message);
            }
        }
        Ok(())
    }

    fn apply(&mut self, message: Incoming) {
        match message {
            Incoming::DeviceList(devices) => {
                self.state.devices = devices.into_iter().map(|d| (d.index, d)).collect();
            }
            Incoming::DeviceAdded(device) => {
                self.state.devices.insert(device.index, device);
            }
            Incoming::DeviceRemoved(index) => {
                self.state.devices.remove(&index);
                self.state.sent.retain(|(device, _), _| *device != index);
                self.state.sent_at.remove(&index);
            }
            Incoming::Error(why) => kopuz_plugin_sdk::warn!("Intiface reported: {why}"),
            Incoming::ServerInfo { .. } => {}
        }
    }

    fn ping_if_due(&mut self) -> Result<(), String> {
        let Some(every) = self.ping_every else {
            return Ok(());
        };
        self.calls += 1;
        if self.calls < every {
            return Ok(());
        }
        self.calls = 0;
        let ping = protocol::ping(self.take_id());
        self.send(&ping)
    }

    /// Reads what the server sent, for a panel that only looks.
    pub fn poll(&mut self) -> Result<(), String> {
        self.drain()
    }

    /// Reads the server, keeps it pinged and sends each device whose step
    /// changed, within its timing gap, at the value `value_for` gives for its
    /// key (0 to 1; `None` leaves it alone): one command per device.
    pub fn drive(&mut self, mut value_for: impl FnMut(&str) -> Option<f32>) -> Result<(), String> {
        self.drain()?;
        self.ping_if_due()?;

        let mut commands = Vec::new();
        let now = Instant::now();
        let keys = self.state.keys();
        for device in self.state.devices.values() {
            if !device.supported() || !self.state.gap_passed(device, now) {
                continue;
            }
            let Some(value) = value_for(&keys[&device.index]) else {
                continue;
            };
            let steps: Vec<u32> = device
                .actuators
                .iter()
                .map(|actuator| quantize(value, actuator.steps))
                .collect();
            let changed = device.actuators.iter().zip(&steps).any(|(actuator, step)| {
                self.state.sent_step(device.index, actuator.index) != *step
            });
            if !changed {
                continue;
            }

            // Every actuator goes in the one command, all at the same value.
            let mut scalars = Vec::new();
            for (actuator, step) in device.actuators.iter().zip(&steps) {
                self.state
                    .sent
                    .insert((device.index, actuator.index), *step);
                scalars.push(Scalar {
                    index: actuator.index,
                    kind: actuator.kind.clone(),
                    value: *step as f64 / actuator.steps as f64,
                });
            }
            self.state.sent_at.insert(device.index, now);
            commands.push((device.index, scalars));
        }
        for (device, scalars) in commands {
            let command = protocol::scalar_cmd(self.take_id(), device, &scalars);
            self.send(&command)?;
        }
        Ok(())
    }

    pub fn scan(&mut self) -> Result<(), String> {
        let scan = protocol::start_scanning(self.take_id());
        self.send(&scan)
    }

    pub fn stop_device(&mut self, index: u32) -> Result<(), String> {
        self.state.sent.retain(|(device, _), _| *device != index);
        self.state.sent_at.remove(&index);
        let stop = protocol::stop_device(self.take_id(), index);
        self.send(&stop)
    }

    /// Drives one device at `value` for `hold`, then stops it; the stop goes
    /// out even when the drive failed.
    pub fn test(&mut self, index: u32, value: f32, hold: Duration) -> Result<(), String> {
        let driven = self
            .drive_one(index, value)
            .map(|()| std::thread::sleep(hold));
        let stopped = self.stop_device(index);
        driven.and(stopped)
    }

    fn drive_one(&mut self, index: u32, value: f32) -> Result<(), String> {
        let Some(device) = self.state.devices.get(&index).filter(|d| d.supported()) else {
            return Err("that device cannot be driven".to_string());
        };
        let mut scalars = Vec::new();
        for actuator in &device.actuators {
            let step = quantize(value, actuator.steps);
            self.state.sent.insert((index, actuator.index), step);
            scalars.push(Scalar {
                index: actuator.index,
                kind: actuator.kind.clone(),
                value: step as f64 / actuator.steps as f64,
            });
        }
        let command = protocol::scalar_cmd(self.take_id(), index, &scalars);
        self.send(&command)
    }

    pub fn stop_all(&mut self) -> Result<(), String> {
        self.state.sent.clear();
        self.state.sent_at.clear();
        let stop = protocol::stop_all(self.take_id());
        self.send(&stop)
    }
}

fn wait_for_info<L: Link>(socket: &mut WebSocket<L>) -> Result<u32, String> {
    for _ in 0..INFO_TRIES {
        let Some(text) = socket.next_text(INFO_WAIT).map_err(|e| e.to_string())? else {
            continue;
        };
        for message in protocol::parse(&text) {
            match message {
                Incoming::ServerInfo { max_ping_ms } => return Ok(max_ping_ms),
                Incoming::Error(why) => return Err(format!("the server refused: {why}")),
                _ => {}
            }
        }
    }
    Err("the server did not answer the handshake".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(index: u32, name: &str) -> Device {
        Device {
            index,
            name: name.into(),
            actuators: Vec::new(),
            timing_gap_ms: 100,
        }
    }

    #[test]
    fn devices_sharing_a_name_are_told_apart_by_index() {
        let mut state = DeviceState::default();
        for (index, name) in [(0, "Toy"), (1, "Toy"), (2, "Other"), (3, "")] {
            state.devices.insert(index, device(index, name));
        }
        let keys: Vec<String> = state.keys().into_values().collect();
        assert_eq!(keys, ["Toy#0", "Toy#1", "Other", "Device"]);
        assert_eq!(state.index_of("Toy#1"), Some(1));
        assert_eq!(state.index_of("Toy"), None);
    }
}
