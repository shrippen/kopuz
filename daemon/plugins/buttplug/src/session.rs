//! One live Buttplug connection: handshake, the device list, and commands
//! that only go out when a device's step changed.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::follow::quantize;
use crate::protocol::{self, Device, Incoming, Scalar, Stroke};
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

/// A stroke covers the time to the next beat, guessed from the last interval.
const STROKE_MIN_MS: u128 = 250;
const STROKE_MAX_MS: u128 = 1000;
const STROKE_DEFAULT_MS: u32 = 500;
/// Where a stroker's strokes are centred.
const MIDDLE: f64 = 0.5;

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
    /// Per (device, axis): the position last sent when following a level, the
    /// stroke length when following the beat; absent means 0.
    pub axes: BTreeMap<(u32, u32), f32>,
    /// Per device: when its last beat stroke went out and whether it ended high.
    pub beating: BTreeMap<u32, (Instant, bool)>,
}

/// Whether a device follows the beat, and if so whether one just came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Beat {
    Off,
    Wait,
    Hit,
}

/// What one device is asked to do on one call.
#[derive(Debug, Clone, Copy)]
pub struct Drive {
    /// What it follows, from 0 to 1, with the strength applied.
    pub value: f32,
    /// The strength alone: how long a stroke on the beat is.
    pub strength: f32,
    pub beat: Beat,
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

    /// The last value sent to a device, from 0 to 1: its strongest actuator,
    /// or for an axis its position (following a level) or stroke length (on
    /// the beat).
    pub fn now(&self, device: &Device) -> f32 {
        let scalars = device.actuators.iter().map(|actuator| {
            self.sent_step(device.index, actuator.index) as f32 / actuator.steps as f32
        });
        let axes = device.axes.iter().map(|axis| {
            self.axes
                .get(&(device.index, axis.index))
                .copied()
                .unwrap_or(0.0)
        });
        scalars.chain(axes).fold(0.0, f32::max)
    }

    /// Forgets what was sent to a device, as when it stopped or went away.
    fn forget(&mut self, index: u32) {
        self.sent.retain(|(device, _), _| *device != index);
        self.axes.retain(|(device, _), _| *device != index);
        self.sent_at.remove(&index);
        self.beating.remove(&index);
    }

    fn forget_all(&mut self) {
        self.sent.clear();
        self.axes.clear();
        self.sent_at.clear();
        self.beating.clear();
    }

    /// The scalars to send so `device` sits at `value`; none when no step changed.
    fn scalars(&mut self, device: &Device, value: f32) -> Vec<Scalar> {
        let steps: Vec<u32> = device
            .actuators
            .iter()
            .map(|actuator| quantize(value, actuator.steps))
            .collect();
        let changed = device
            .actuators
            .iter()
            .zip(&steps)
            .any(|(actuator, step)| self.sent_step(device.index, actuator.index) != *step);
        if !changed {
            return Vec::new();
        }

        // Every actuator goes in the one command, all at the same value.
        let mut scalars = Vec::new();
        for (actuator, step) in device.actuators.iter().zip(&steps) {
            self.sent.insert((device.index, actuator.index), *step);
            scalars.push(Scalar {
                index: actuator.index,
                kind: actuator.kind.clone(),
                value: *step as f64 / actuator.steps as f64,
            });
        }
        scalars
    }

    /// The strokes to send for `drive`: a level moves the axes to it over the
    /// device's timing gap, a beat swings them to the other end.
    fn strokes(&mut self, device: &Device, drive: &Drive, now: Instant) -> Vec<Stroke> {
        match drive.beat {
            Beat::Off => self.follow_strokes(device, drive.value),
            Beat::Wait => Vec::new(),
            Beat::Hit => self.beat_strokes(device, drive.strength, now),
        }
    }

    fn follow_strokes(&mut self, device: &Device, value: f32) -> Vec<Stroke> {
        let mut strokes = Vec::new();
        for axis in &device.axes {
            let step = quantize(value, axis.steps);
            let before = self.axes.get(&(device.index, axis.index)).copied();
            if quantize(before.unwrap_or(0.0), axis.steps) == step {
                continue;
            }
            let position = step as f32 / axis.steps as f32;
            self.axes.insert((device.index, axis.index), position);
            strokes.push(Stroke {
                index: axis.index,
                position: position.into(),
                duration_ms: device.timing_gap_ms,
            });
        }
        strokes
    }

    /// Swings each axis to the end it is not at (high first), over about the
    /// time since the last beat, which is when the next is expected.
    fn beat_strokes(&mut self, device: &Device, strength: f32, now: Instant) -> Vec<Stroke> {
        let (last, was_high) = self.beating.get(&device.index).copied().unzip();
        let duration_ms = last.map_or(STROKE_DEFAULT_MS, |at| {
            now.duration_since(at)
                .as_millis()
                .clamp(STROKE_MIN_MS, STROKE_MAX_MS) as u32
        });
        let high = !was_high.unwrap_or(false);
        self.beating.insert(device.index, (now, high));

        let half = f64::from(strength) / 2.0;
        let position = if high { MIDDLE + half } else { MIDDLE - half };
        let mut strokes = Vec::new();
        for axis in &device.axes {
            self.axes.insert((device.index, axis.index), strength);
            strokes.push(Stroke {
                index: axis.index,
                position,
                duration_ms,
            });
        }
        strokes
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
                self.state.forget(index);
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

    /// Reads the server, keeps it pinged and sends each device what
    /// `drive_for` gives for its key (`None` leaves it alone), within its
    /// timing gap: a scalar command when a step changed, strokes for its axes.
    pub fn drive(
        &mut self,
        mut drive_for: impl FnMut(&str) -> Option<Drive>,
    ) -> Result<(), String> {
        self.drain()?;
        self.ping_if_due()?;

        let mut commands = Vec::new();
        let now = Instant::now();
        let keys = self.state.keys();
        let devices: Vec<Device> = self.state.devices.values().cloned().collect();
        for device in devices {
            if !device.supported() || !self.state.gap_passed(&device, now) {
                continue;
            }
            let Some(drive) = drive_for(&keys[&device.index]) else {
                continue;
            };
            let scalars = self.state.scalars(&device, drive.value);
            let strokes = self.state.strokes(&device, &drive, now);
            if scalars.is_empty() && strokes.is_empty() {
                continue;
            }
            self.state.sent_at.insert(device.index, now);
            commands.push((device.index, scalars, strokes));
        }
        for (device, scalars, strokes) in commands {
            self.send_scalars(device, &scalars)?;
            self.send_strokes(device, &strokes)?;
        }
        Ok(())
    }

    fn send_scalars(&mut self, device: u32, scalars: &[Scalar]) -> Result<(), String> {
        if scalars.is_empty() {
            return Ok(());
        }
        let command = protocol::scalar_cmd(self.take_id(), device, scalars);
        self.send(&command)
    }

    fn send_strokes(&mut self, device: u32, strokes: &[Stroke]) -> Result<(), String> {
        if strokes.is_empty() {
            return Ok(());
        }
        let command = protocol::linear_cmd(self.take_id(), device, strokes);
        self.send(&command)
    }

    pub fn scan(&mut self) -> Result<(), String> {
        let scan = protocol::start_scanning(self.take_id());
        self.send(&scan)
    }

    pub fn stop_device(&mut self, index: u32) -> Result<(), String> {
        self.state.forget(index);
        let stop = protocol::stop_device(self.take_id(), index);
        self.send(&stop)
    }

    /// Drives one device at `strength` for `hold`, then stops it; the stop
    /// goes out even when the drive failed. An axis strokes out and back, each
    /// in half of `hold`.
    pub fn test(&mut self, index: u32, strength: f32, hold: Duration) -> Result<(), String> {
        let driven = self.test_drive(index, strength, hold);
        let stopped = self.stop_device(index);
        driven.and(stopped)
    }

    fn test_drive(&mut self, index: u32, strength: f32, hold: Duration) -> Result<(), String> {
        let Some(device) = self.state.devices.get(&index).filter(|d| d.supported()) else {
            return Err("that device cannot be driven".to_string());
        };
        let device = device.clone();
        let scalars = self.state.scalars(&device, strength);
        self.send_scalars(index, &scalars)?;

        let half = hold / 2;
        let duration_ms = half.as_millis() as u32;
        let half_stroke = f64::from(strength) / 2.0;
        for position in [MIDDLE + half_stroke, MIDDLE - half_stroke] {
            let strokes: Vec<Stroke> = device
                .axes
                .iter()
                .map(|axis| Stroke {
                    index: axis.index,
                    position,
                    duration_ms,
                })
                .collect();
            self.send_strokes(index, &strokes)?;
            std::thread::sleep(half);
        }
        Ok(())
    }

    pub fn stop_all(&mut self) -> Result<(), String> {
        self.state.forget_all();
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
            axes: Vec::new(),
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
