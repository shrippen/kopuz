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

/// What a panel can show: whether Intiface is connected, its devices and
/// what was last sent to them.
#[derive(Debug, Default, Clone)]
pub struct DeviceState {
    // Read by the panel, which is yet to come.
    #[allow(dead_code)]
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

    /// Reads the server, keeps it pinged and sends each device whose step
    /// changed, within its timing gap, at `value` (0 to 1): one command per device.
    pub fn drive(&mut self, value: f32) -> Result<(), String> {
        self.drain()?;
        self.ping_if_due()?;

        let mut commands = Vec::new();
        let now = Instant::now();
        for device in self.state.devices.values() {
            if !device.supported() || !self.state.gap_passed(device, now) {
                continue;
            }
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
