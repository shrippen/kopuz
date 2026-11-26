//! The plugin's logic over any way of connecting: follows the music, keeps a
//! session open and reopens it after an error.

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kopuz_plugin_sdk::SourceError;
use kopuz_plugin_sdk::integration::Section;

use crate::follow::{Follow, Followers, Levels, Sample};
use crate::panel::{self, Link as LinkStatus};
use crate::prefs::{Action, Prefs};
use crate::session::{Beat, Drive, Session};
use crate::ws::{Endpoint, Link};

/// Calls (about 50 ms each) to wait after a failure, doubling up to the max.
const BACKOFF_MIN: u32 = 40;
const BACKOFF_MAX: u32 = 600;
/// A panel poll comes about every 200 ms, four audio calls' worth.
const PANEL_TICKS: u32 = 4;
/// How long a device's test button drives it.
const TEST_HOLD: Duration = Duration::from_millis(500);

pub trait Connect {
    type Link: Link;

    fn connect(&self, endpoint: &Endpoint) -> io::Result<Self::Link>;
}

pub struct Engine<C: Connect> {
    connector: C,
    endpoint: Endpoint,
    followers: Followers,
    prefs: Prefs,
    session: Option<Session<C::Link>>,
    /// Calls left before the next attempt to connect.
    wait: u32,
    penalty: u32,
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0x9E37_79B9, |since| since.as_nanos() as u64)
}

impl<C: Connect> Engine<C> {
    pub fn new(connector: C, endpoint: Endpoint) -> Self {
        Engine {
            connector,
            endpoint,
            followers: Followers::default(),
            prefs: Prefs::default(),
            session: None,
            wait: 0,
            penalty: 0,
        }
    }

    fn open(&self) -> Result<Session<C::Link>, String> {
        let link = self
            .connector
            .connect(&self.endpoint)
            .map_err(|e| e.to_string())?;
        Session::open(link, &self.endpoint, seed())
    }

    /// Whether a server answers at the address; the connection is dropped again.
    pub fn check(&self) -> Result<(), String> {
        self.open().map(drop)
    }

    fn fail(&mut self, why: &str) {
        kopuz_plugin_sdk::warn!("Intiface Central: {why}");
        self.session = None;
        self.penalty = (self.penalty * 2).clamp(BACKOFF_MIN, BACKOFF_MAX);
        self.wait = self.penalty;
    }

    /// Makes sure a session is open, if the backoff allows; each call counts
    /// `ticks` audio calls towards the end of the backoff.
    fn ensure(&mut self, ticks: u32) -> bool {
        if self.session.is_some() {
            return true;
        }
        if self.wait > 0 {
            self.wait = self.wait.saturating_sub(ticks);
            return false;
        }
        self.connect_now()
    }

    /// Opens a session now, whatever the backoff says.
    fn connect_now(&mut self) -> bool {
        if self.session.is_some() {
            return true;
        }
        match self.open() {
            Ok(session) => {
                self.session = Some(session);
                self.penalty = 0;
                true
            }
            Err(why) => {
                self.fail(&why);
                false
            }
        }
    }

    /// Drives the devices to follow `samples`.
    pub fn audio(&mut self, samples: &[Sample]) {
        let levels = self.followers.feed(samples);
        if !self.ensure(1) {
            return;
        }

        let prefs = &self.prefs;
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Err(why) = session.drive(|key| drive_for(prefs, &levels, key)) {
            self.fail(&why);
        }
    }

    /// What to show; reads the server and connects when the backoff allows,
    /// but never drives a device.
    pub fn panel(&mut self) -> Vec<Section> {
        if self.ensure(PANEL_TICKS) {
            let polled = self.session.as_mut().map(Session::poll);
            if let Some(Err(why)) = polled {
                self.fail(&why);
            }
        }
        let link = match &self.session {
            Some(session) if session.state.connected => LinkStatus::Connected,
            Some(_) => LinkStatus::Connecting,
            None if self.penalty == 0 => LinkStatus::Connecting,
            None => LinkStatus::Unreachable,
        };
        let state = self.session.as_ref().map(|session| &session.state);
        panel::sections(link, &self.prefs, state)
    }

    /// Takes a change from the panel.
    pub fn act(
        &mut self,
        section: &str,
        control: &str,
        value: f32,
        text: &str,
    ) -> Result<(), SourceError> {
        match self.prefs.act(section, control, value, text)? {
            Action::Nothing => {}
            Action::StopAll => self.stop(),
            Action::StopDevice(key) => self.stop_device(&key),
            Action::Scan => return self.scan(),
            Action::Test(key) => return self.test(&key),
        }
        Ok(())
    }

    fn unreachable() -> SourceError {
        SourceError::Backend("Intiface Central is not reachable. Start its server.".into())
    }

    fn scan(&mut self) -> Result<(), SourceError> {
        if !self.connect_now() {
            return Err(Self::unreachable());
        }
        let Some(session) = self.session.as_mut() else {
            return Err(Self::unreachable());
        };
        session.scan().map_err(|why| {
            self.fail(&why);
            SourceError::Backend(why)
        })
    }

    /// Stops one device, if it is connected.
    fn stop_device(&mut self, key: &str) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let Some(index) = session.state.index_of(key) else {
            return;
        };
        if let Err(why) = session.stop_device(index) {
            self.fail(&why);
        }
    }

    /// Drives one device at the master times its own cap for a moment.
    fn test(&mut self, key: &str) -> Result<(), SourceError> {
        let device = self.prefs.device(key);
        if !self.prefs.active || !device.on {
            return Err(SourceError::InvalidInput(
                "turn Active and the device on first".into(),
            ));
        }
        if !self.connect_now() {
            return Err(Self::unreachable());
        }

        let value = self.prefs.master * device.cap;
        let Some(session) = self.session.as_mut() else {
            return Err(Self::unreachable());
        };
        let Some(index) = session.state.index_of(key) else {
            return Err(SourceError::InvalidInput(
                "that device is not connected".into(),
            ));
        };
        session
            .test(index, value, TEST_HOLD)
            .map_err(SourceError::Backend)
    }

    /// Stops every device, for a pause or the end of the music.
    pub fn stop(&mut self) {
        self.followers.reset();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Err(why) = session.stop_all() {
            self.fail(&why);
        }
    }
}

/// How a device is driven now; `None` when it must not be.
fn drive_for(prefs: &Prefs, levels: &Levels, key: &str) -> Option<Drive> {
    let device = prefs.device(key);
    if !prefs.active || !device.on {
        return None;
    }
    let follow = prefs.follows(key);
    let strength = prefs.master * device.cap;
    let beat = match (follow, levels.hit()) {
        (Follow::Beat, true) => Beat::Hit,
        (Follow::Beat, false) => Beat::Wait,
        _ => Beat::Off,
    };
    Some(Drive {
        value: levels.get(follow) * strength,
        strength,
        beat,
    })
}

impl<C: Connect> Drop for Engine<C> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::prefs;
    use crate::ws::{OP_TEXT, decode_frame, encode_frame};
    use kopuz_plugin_sdk::integration::{Control, ControlKind, Section};
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    struct StdConnect;

    impl Connect for StdConnect {
        type Link = TcpStream;

        fn connect(&self, endpoint: &Endpoint) -> io::Result<TcpStream> {
            TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        }
    }

    const DEVICE: &str = r#"{"DeviceIndex":0,"DeviceMessageTimingGap":100,"DeviceMessages":{"ScalarCmd":[{"ActuatorType":"Vibrate","FeatureDescriptor":"Motor 1","StepCount":100},{"ActuatorType":"Oscillate","FeatureDescriptor":"Motor 2","StepCount":100}],"StopDeviceCmd":{}},"DeviceName":"Simulated 2-Motor Vibrator"}"#;

    const STROKER: &str = r#"{"DeviceIndex":1,"DeviceMessageTimingGap":100,"DeviceMessages":{"LinearCmd":[{"ActuatorType":"Position","FeatureDescriptor":"Linear Axis","StepCount":1000}],"ScalarCmd":[{"ActuatorType":"Position","FeatureDescriptor":"Linear Axis","StepCount":1000}],"StopDeviceCmd":{}},"DeviceName":"Simulated Stroker"}"#;
    const VIBRATOR: &str = "Simulated 2-Motor Vibrator";
    const STROKER_NAME: &str = "Simulated Stroker";

    /// A device with a vibrator and a position axis.
    const HYBRID: &str = r#"{"DeviceIndex":2,"DeviceMessageTimingGap":100,"DeviceMessages":{"LinearCmd":[{"ActuatorType":"Position","FeatureDescriptor":"Axis","StepCount":1000}],"ScalarCmd":[{"ActuatorType":"Vibrate","FeatureDescriptor":"Motor","StepCount":100}],"StopDeviceCmd":{}},"DeviceName":"Hybrid"}"#;

    fn reply(stream: &mut TcpStream, body: Value) {
        let text = Value::Array(vec![body]).to_string();
        stream
            .write_all(&encode_frame(OP_TEXT, text.as_bytes(), None))
            .unwrap();
    }

    /// A server that upgrades, answers the handshake and the device request,
    /// and reports each message it receives (as its type name and body).
    fn serve(listener: TcpListener) -> Receiver<(String, Value)> {
        serve_with(listener, &[DEVICE])
    }

    /// Like `serve`, with these devices in its list.
    fn serve_with(listener: TcpListener, devices: &[&str]) -> Receiver<(String, Value)> {
        let devices: Vec<Value> = devices
            .iter()
            .map(|device| serde_json::from_str(device).unwrap())
            .collect();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut upgraded = false;
            loop {
                let n = match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);

                if !upgraded {
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    buf.drain(..end + 4);
                    let head = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";
                    stream.write_all(head.as_bytes()).unwrap();
                    upgraded = true;
                }
                while let Some((frame, used)) = decode_frame(&buf).unwrap() {
                    buf.drain(..used);
                    let packet: Value = serde_json::from_slice(&frame.payload).unwrap();
                    let (kind, body) = packet[0].as_object().unwrap().iter().next().unwrap();
                    match kind.as_str() {
                        "RequestServerInfo" => reply(
                            &mut stream,
                            json!({"ServerInfo": {"Id": 1, "MessageVersion": 3, "MaxPingTime": 0}}),
                        ),
                        "RequestDeviceList" => reply(
                            &mut stream,
                            json!({"DeviceList": {"Id": 3, "Devices": devices}}),
                        ),
                        _ => {}
                    }
                    let _ = tx.send((kind.clone(), body.clone()));
                }
            }
        });
        rx
    }

    /// An engine following `follow` (a choice value) at `percent` strength.
    fn engine_for(port: u16, follow: &str, percent: f32) -> Engine<StdConnect> {
        let endpoint = Endpoint::parse(&format!("ws://127.0.0.1:{port}")).unwrap();
        let mut engine = Engine::new(StdConnect, endpoint);
        engine.act("", prefs::STRENGTH, percent, "").unwrap();
        engine.act("", prefs::FOLLOW, 0.0, follow).unwrap();
        engine
    }

    fn loud() -> [Sample; 1] {
        [Sample {
            level: 0.5,
            low: 0.5,
            beat: false,
        }]
    }

    fn next(rx: &Receiver<(String, Value)>) -> (String, Value) {
        rx.recv_timeout(Duration::from_secs(5)).expect("a message")
    }

    #[test]
    fn the_client_shakes_hands_follows_the_music_and_stops() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 50.0);

        engine.audio(&loud());
        assert_eq!(next(&rx).0, "RequestServerInfo");
        assert_eq!(next(&rx).0, "StartScanning");
        assert_eq!(next(&rx).0, "RequestDeviceList");

        // The device list is read on a later call; then the command goes out.
        let mut command = None;
        for _ in 0..200 {
            engine.audio(&loud());
            if let Ok((kind, body)) = rx.recv_timeout(Duration::from_millis(20)) {
                command = Some((kind, body));
                break;
            }
        }
        let (kind, body) = command.expect("a ScalarCmd");
        assert_eq!(kind, "ScalarCmd");
        assert_eq!(body["DeviceIndex"], 0);
        let scalars = body["Scalars"].as_array().unwrap();
        assert_eq!(scalars.len(), 2, "one command for both motors");
        assert_eq!(scalars[0]["ActuatorType"], "Vibrate");
        assert_eq!(scalars[1]["ActuatorType"], "Oscillate");
        assert_eq!(scalars[0]["Scalar"], scalars[1]["Scalar"]);
        let scalar = scalars[0]["Scalar"].as_f64().unwrap();
        assert!(
            scalar > 0.0 && scalar <= 0.5,
            "capped at the strength: {scalar}"
        );

        // Once the envelope has settled, the same step sends nothing.
        for _ in 0..30 {
            engine.audio(&loud());
            std::thread::sleep(Duration::from_millis(10));
        }
        while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
        for _ in 0..5 {
            engine.audio(&loud());
        }
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

        engine.stop();
        assert_eq!(next(&rx).0, "StopAllDevices");
    }

    #[test]
    fn a_device_is_not_sent_commands_faster_than_its_gap() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 100.0);

        let mut first = false;
        for _ in 0..200 {
            engine.audio(&loud());
            if rx
                .recv_timeout(Duration::from_millis(20))
                .is_ok_and(|(k, _)| k == "ScalarCmd")
            {
                first = true;
                break;
            }
        }
        assert!(first, "a first ScalarCmd");

        // The envelope still rises, so each call wants a new step, but the
        // device was just sent one.
        for _ in 0..3 {
            engine.audio(&loud());
        }
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
    }

    #[test]
    fn dropping_the_engine_stops_the_devices() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "beat", 100.0);
        engine.audio(&loud());
        drop(engine);

        let kinds: Vec<String> =
            std::iter::from_fn(|| rx.recv_timeout(Duration::from_secs(2)).ok())
                .map(|(kind, _)| kind)
                .take(4)
                .collect();
        assert_eq!(
            kinds.last().map(String::as_str),
            Some("StopAllDevices"),
            "{kinds:?}"
        );
    }

    #[test]
    fn an_unreachable_server_fails_the_check_and_backs_off() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut engine = engine_for(port, "level", 50.0);
        assert!(engine.check().is_err());

        engine.audio(&loud());
        assert_eq!(engine.wait, BACKOFF_MIN);
        engine.audio(&loud());
        assert_eq!(engine.wait, BACKOFF_MIN - 1);
    }

    fn nap() {
        std::thread::sleep(Duration::from_millis(1));
    }

    /// Polls the panel until `devices` devices are listed; returns it.
    fn panel_with(engine: &mut Engine<StdConnect>, devices: usize) -> Vec<Section> {
        for _ in 0..500 {
            let sections = engine.panel();
            if sections.len() == devices + 1 {
                return sections;
            }
            nap();
        }
        panic!("no {devices} devices in the panel");
    }

    /// Everything the server heard within a short wait.
    fn heard(rx: &Receiver<(String, Value)>) -> Vec<(String, Value)> {
        std::iter::from_fn(|| rx.recv_timeout(Duration::from_millis(100)).ok()).collect()
    }

    fn kinds(heard: &[(String, Value)]) -> Vec<&str> {
        heard.iter().map(|(kind, _)| kind.as_str()).collect()
    }

    /// Calls `audio` until a ScalarCmd arrives; its first scalar.
    fn first_scalar(engine: &mut Engine<StdConnect>, rx: &Receiver<(String, Value)>) -> f64 {
        for _ in 0..200 {
            engine.audio(&loud());
            while let Ok((kind, body)) = rx.recv_timeout(Duration::from_millis(20)) {
                if kind == "ScalarCmd" {
                    return body["Scalars"][0]["Scalar"].as_f64().unwrap();
                }
            }
        }
        panic!("no ScalarCmd");
    }

    /// Whether `audio` sends a ScalarCmd within a number of calls.
    fn drives(engine: &mut Engine<StdConnect>, rx: &Receiver<(String, Value)>) -> bool {
        for _ in 0..40 {
            engine.audio(&loud());
            if heard_scalar(rx) {
                return true;
            }
        }
        false
    }

    fn heard_scalar(rx: &Receiver<(String, Value)>) -> bool {
        std::iter::from_fn(|| rx.recv_timeout(Duration::from_millis(5)).ok())
            .any(|(kind, _)| kind == "ScalarCmd")
    }

    fn control<'a>(section: &'a Section, key: &str) -> &'a Control {
        section.controls.iter().find(|c| c.key == key).expect(key)
    }

    #[test]
    fn the_panel_lists_the_plugin_and_each_device_and_never_drives() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve_with(listener, &[DEVICE, STROKER]);
        let mut engine = engine_for(port, "level", 50.0);

        let sections = panel_with(&mut engine, 2);
        let plugin = &sections[0];
        assert_eq!(plugin.key, "");
        assert_eq!(plugin.title, "");
        assert_eq!(control(plugin, prefs::STATUS).text, "Connected");
        assert_eq!(control(plugin, prefs::ACTIVE).value, 1.0);
        assert_eq!(control(plugin, prefs::STRENGTH).value, 50.0);
        assert_eq!(control(plugin, prefs::FOLLOW).text, "level");
        assert!(matches!(
            control(plugin, prefs::SCAN).kind,
            ControlKind::Button
        ));

        let vibrator = &sections[1];
        assert_eq!(
            (vibrator.key.as_str(), vibrator.title.as_str()),
            (VIBRATOR, VIBRATOR)
        );
        let keys: Vec<&str> = vibrator.controls.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                prefs::NOW,
                prefs::ON,
                prefs::STRENGTH,
                prefs::FOLLOWS,
                prefs::TEST
            ]
        );
        assert_eq!(control(vibrator, prefs::ON).value, 1.0);
        assert_eq!(control(vibrator, prefs::STRENGTH).value, 100.0);
        assert_eq!(control(vibrator, prefs::FOLLOWS).text, "");

        let stroker = &sections[2];
        assert_eq!(stroker.key, STROKER_NAME);
        let keys: Vec<&str> = stroker.controls.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                prefs::NOW,
                prefs::ON,
                prefs::STRENGTH,
                prefs::FOLLOWS,
                prefs::TEST
            ]
        );

        let sent = heard(&rx);
        assert_eq!(
            kinds(&sent),
            ["RequestServerInfo", "StartScanning", "RequestDeviceList"]
        );
    }

    #[test]
    fn an_unreachable_server_shows_in_the_panel() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut engine = engine_for(port, "level", 50.0);
        let sections = engine.panel();
        assert_eq!(sections.len(), 1);
        assert_eq!(
            control(&sections[0], prefs::STATUS).text,
            "Not reachable — start its server"
        );
    }

    #[test]
    fn the_master_strength_caps_what_is_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 100.0);
        engine.act("", prefs::STRENGTH, 20.0, "").unwrap();

        let scalar = first_scalar(&mut engine, &rx);
        assert!(scalar > 0.0 && scalar <= 0.2, "{scalar}");
    }

    #[test]
    fn a_device_strength_applies_on_top_of_the_master() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 100.0);
        engine.act(VIBRATOR, prefs::STRENGTH, 40.0, "").unwrap();

        let scalar = first_scalar(&mut engine, &rx);
        assert!(scalar > 0.0 && scalar <= 0.4, "{scalar}");
    }

    #[test]
    fn a_setting_acted_before_the_device_appears_applies_to_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 100.0);

        // As the host replays them: no connection, no devices yet.
        engine.act(VIBRATOR, prefs::STRENGTH, 20.0, "").unwrap();
        engine.act(VIBRATOR, prefs::FOLLOWS, 0.0, "low").unwrap();
        assert!(heard(&rx).is_empty());

        let sections = panel_with(&mut engine, 1);
        assert_eq!(control(&sections[1], prefs::STRENGTH).value, 20.0);
        assert_eq!(control(&sections[1], prefs::FOLLOWS).text, "low");
        let scalar = first_scalar(&mut engine, &rx);
        assert!(scalar > 0.0 && scalar <= 0.2, "{scalar}");
    }

    #[test]
    fn a_device_follows_its_own_source() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 100.0);

        // A beat never comes in `loud`, so a device following it stays still.
        engine.act(VIBRATOR, prefs::FOLLOWS, 0.0, "beat").unwrap();
        assert!(!drives(&mut engine, &rx));

        engine.act(VIBRATOR, prefs::FOLLOWS, 0.0, "").unwrap();
        assert!(drives(&mut engine, &rx));
    }

    #[test]
    fn a_device_switched_off_is_stopped_and_skipped() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve_with(listener, &[DEVICE, STROKER]);
        let mut engine = engine_for(port, "level", 100.0);
        panel_with(&mut engine, 2);
        heard(&rx);

        engine.act(VIBRATOR, prefs::ON, 0.0, "").unwrap();
        let sent = heard(&rx);
        assert_eq!(kinds(&sent), ["StopDeviceCmd"]);
        assert_eq!(sent[0].1["DeviceIndex"], 0);

        assert!(!drives(&mut engine, &rx));
        engine.act(VIBRATOR, prefs::ON, 1.0, "").unwrap();
        assert!(drives(&mut engine, &rx));
    }

    #[test]
    fn switching_active_off_stops_everything_until_on_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 100.0);
        first_scalar(&mut engine, &rx);

        engine.act("", prefs::ACTIVE, 0.0, "").unwrap();
        assert_eq!(kinds(&heard(&rx)), ["StopAllDevices"]);
        assert!(!drives(&mut engine, &rx));

        engine.act("", prefs::ACTIVE, 1.0, "").unwrap();
        assert!(drives(&mut engine, &rx));
    }

    #[test]
    fn the_test_button_drives_one_device_and_stops_just_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve_with(listener, &[DEVICE, STROKER]);
        let mut engine = engine_for(port, "level", 50.0);
        panel_with(&mut engine, 2);
        heard(&rx);

        engine.act(VIBRATOR, prefs::STRENGTH, 50.0, "").unwrap();
        engine.act(VIBRATOR, prefs::TEST, 0.0, "").unwrap();
        let sent = heard(&rx);
        assert_eq!(kinds(&sent), ["ScalarCmd", "StopDeviceCmd"]);
        assert_eq!(sent[0].1["DeviceIndex"], 0);
        assert_eq!(sent[0].1["Scalars"][0]["Scalar"], 0.25);
        assert_eq!(sent[1].1["DeviceIndex"], 0);
    }

    #[test]
    fn the_test_button_needs_a_reachable_server_and_a_known_device() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut engine = engine_for(port, "level", 50.0);
        assert!(matches!(
            engine.act(VIBRATOR, prefs::TEST, 0.0, ""),
            Err(SourceError::Backend(_))
        ));
        assert!(matches!(
            engine.act("", prefs::SCAN, 0.0, ""),
            Err(SourceError::Backend(_))
        ));
    }

    #[test]
    fn scanning_connects_and_asks_for_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve(listener);
        let mut engine = engine_for(port, "level", 50.0);

        engine.act("", prefs::SCAN, 0.0, "").unwrap();
        let sent = heard(&rx);
        assert_eq!(
            kinds(&sent),
            [
                "RequestServerInfo",
                "StartScanning",
                "RequestDeviceList",
                "StartScanning"
            ]
        );
    }

    #[test]
    fn invalid_values_are_refused() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut engine = engine_for(port, "level", 50.0);
        for (section, control, value, text) in [
            ("", prefs::STRENGTH, 150.0, ""),
            ("", prefs::FOLLOW, 0.0, "bogus"),
            ("", "bogus", 0.0, ""),
            (VIBRATOR, prefs::STRENGTH, -5.0, ""),
            (VIBRATOR, prefs::FOLLOWS, 0.0, "bogus"),
            (VIBRATOR, "bogus", 0.0, ""),
        ] {
            assert!(
                matches!(
                    engine.act(section, control, value, text),
                    Err(SourceError::InvalidInput(_))
                ),
                "{section}/{control}"
            );
        }
    }

    fn beat() -> [Sample; 1] {
        [Sample {
            level: 0.0,
            low: 0.0,
            beat: true,
        }]
    }

    /// The server's next message of this kind, skipping the others.
    fn next_of(rx: &Receiver<(String, Value)>, wanted: &str) -> Value {
        loop {
            let (kind, body) = next(rx);
            if kind == wanted {
                return body;
            }
        }
    }

    /// A connected engine with the stroker listed and the handshake heard.
    fn stroker_engine(
        follow: &str,
        percent: f32,
    ) -> (Engine<StdConnect>, Receiver<(String, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve_with(listener, &[STROKER]);
        let mut engine = engine_for(port, follow, percent);
        panel_with(&mut engine, 1);
        heard(&rx);
        (engine, rx)
    }

    fn position(body: &Value) -> f64 {
        body["Vectors"][0]["Position"].as_f64().unwrap()
    }

    fn duration(body: &Value) -> u64 {
        body["Vectors"][0]["Duration"].as_u64().unwrap()
    }

    #[test]
    fn a_stroker_swings_between_the_ends_on_each_beat() {
        let (mut engine, rx) = stroker_engine("beat", 50.0);

        // No beat, no stroke.
        engine.audio(&loud());
        assert!(heard(&rx).is_empty());

        // The first beat has no interval to go by: the default, to the high end.
        engine.audio(&beat());
        let first = next_of(&rx, "LinearCmd");
        assert_eq!(first["DeviceIndex"], 1);
        assert_eq!(position(&first), 0.75);
        assert_eq!(duration(&first), 500);

        // The next goes to the low end over the time since the last beat.
        std::thread::sleep(Duration::from_millis(300));
        engine.audio(&beat());
        let second = next_of(&rx, "LinearCmd");
        assert_eq!(position(&second), 0.25);
        assert!((300..=400).contains(&duration(&second)), "{second}");

        // A slow beat is clamped to a second.
        std::thread::sleep(Duration::from_millis(1100));
        engine.audio(&beat());
        let third = next_of(&rx, "LinearCmd");
        assert_eq!(position(&third), 0.75);
        assert_eq!(duration(&third), 1000);

        // The panel shows the stroke length.
        let sections = engine.panel();
        assert_eq!(control(&sections[1], prefs::NOW).value, 50.0 / 100.0);
    }

    #[test]
    fn a_stroker_follows_the_level_over_its_timing_gap() {
        let (mut engine, rx) = stroker_engine("level", 100.0);
        let mut body = None;
        for _ in 0..200 {
            engine.audio(&loud());
            if let Ok((kind, found)) = rx.recv_timeout(Duration::from_millis(20)) {
                assert_eq!(kind, "LinearCmd");
                body = Some(found);
                break;
            }
        }
        let body = body.expect("a LinearCmd");
        assert!(position(&body) > 0.0 && position(&body) <= 1.0, "{body}");
        assert_eq!(duration(&body), 100);

        // The envelope still rises, but the device was just sent one.
        for _ in 0..3 {
            engine.audio(&loud());
        }
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
    }

    #[test]
    fn a_stroker_is_skipped_when_off_and_stopped_by_the_buttons() {
        let (mut engine, rx) = stroker_engine("beat", 100.0);

        engine.act(STROKER_NAME, prefs::ON, 0.0, "").unwrap();
        let sent = heard(&rx);
        assert_eq!(kinds(&sent), ["StopDeviceCmd"]);
        assert_eq!(sent[0].1["DeviceIndex"], 1);
        engine.audio(&beat());
        assert!(heard(&rx).is_empty());

        engine.act(STROKER_NAME, prefs::ON, 1.0, "").unwrap();
        engine.audio(&beat());
        assert_eq!(kinds(&heard(&rx)), ["LinearCmd"]);

        engine.stop();
        assert_eq!(kinds(&heard(&rx)), ["StopAllDevices"]);
    }

    #[test]
    fn the_test_button_strokes_a_stroker_out_and_back_then_stops_it() {
        let (mut engine, rx) = stroker_engine("level", 50.0);

        engine.act(STROKER_NAME, prefs::TEST, 0.0, "").unwrap();
        let sent = heard(&rx);
        assert_eq!(kinds(&sent), ["LinearCmd", "LinearCmd", "StopDeviceCmd"]);
        assert_eq!(position(&sent[0].1), 0.75);
        assert_eq!(position(&sent[1].1), 0.25);
        assert_eq!(duration(&sent[0].1), 250);
        assert_eq!(sent[2].1["DeviceIndex"], 1);
    }

    #[test]
    fn a_device_with_a_vibrator_and_an_axis_gets_both_commands() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = serve_with(listener, &[HYBRID]);
        let mut engine = engine_for(port, "level", 100.0);
        panel_with(&mut engine, 1);
        heard(&rx);

        let mut sent = Vec::new();
        for _ in 0..200 {
            engine.audio(&loud());
            sent.extend(heard(&rx));
            if !sent.is_empty() {
                break;
            }
        }
        assert_eq!(kinds(&sent), ["ScalarCmd", "LinearCmd"]);
    }

    #[test]
    fn a_slider_reports_exactly_the_percent_it_was_set_to() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _rx = serve(listener);
        let mut engine = engine_for(port, "level", 50.0);
        panel_with(&mut engine, 1);

        for percent in [60.0, 35.0, 85.0] {
            engine.act("", prefs::STRENGTH, percent, "").unwrap();
            engine.act(VIBRATOR, prefs::STRENGTH, percent, "").unwrap();
            let sections = engine.panel();
            assert_eq!(control(&sections[0], prefs::STRENGTH).value, percent);
            assert_eq!(control(&sections[1], prefs::STRENGTH).value, percent);
        }
    }
}
