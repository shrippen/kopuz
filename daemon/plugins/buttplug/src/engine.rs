//! The plugin's logic over any way of connecting: follows the music, keeps a
//! session open and reopens it after an error.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::follow::{Follow, Follower, Sample};
use crate::session::Session;
use crate::ws::{Endpoint, Link};

/// Calls (about 50 ms each) to wait after a failure, doubling up to the max.
const BACKOFF_MIN: u32 = 40;
const BACKOFF_MAX: u32 = 600;

pub trait Connect {
    type Link: Link;

    fn connect(&self, endpoint: &Endpoint) -> io::Result<Self::Link>;
}

pub struct Engine<C: Connect> {
    connector: C,
    endpoint: Endpoint,
    follower: Follower,
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
    pub fn new(connector: C, endpoint: Endpoint, follow: Follow, cap: f32) -> Self {
        Engine {
            connector,
            endpoint,
            follower: Follower::new(follow, cap),
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

    /// Drives the devices to follow `samples`.
    pub fn audio(&mut self, samples: &[Sample]) {
        let value = self.follower.feed(samples);

        if self.session.is_none() {
            if self.wait > 0 {
                self.wait -= 1;
                return;
            }
            match self.open() {
                Ok(session) => {
                    self.session = Some(session);
                    self.penalty = 0;
                }
                Err(why) => return self.fail(&why),
            }
        }

        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Err(why) = session.drive(value) {
            self.fail(&why);
        }
    }

    /// Stops every device, for a pause or the end of the music.
    pub fn stop(&mut self) {
        self.follower.reset();
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Err(why) = session.stop_all() {
            self.fail(&why);
        }
    }
}

impl<C: Connect> Drop for Engine<C> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ws::{OP_TEXT, decode_frame, encode_frame};
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

    fn reply(stream: &mut TcpStream, body: Value) {
        let text = Value::Array(vec![body]).to_string();
        stream
            .write_all(&encode_frame(OP_TEXT, text.as_bytes(), None))
            .unwrap();
    }

    /// A server that upgrades, answers the handshake and the device request,
    /// and reports each message it receives (as its type name and body).
    fn serve(listener: TcpListener) -> Receiver<(String, Value)> {
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
                            json!({"DeviceList": {"Id": 3, "Devices":
                                [serde_json::from_str::<Value>(DEVICE).unwrap()]}}),
                        ),
                        _ => {}
                    }
                    let _ = tx.send((kind.clone(), body.clone()));
                }
            }
        });
        rx
    }

    fn engine_for(port: u16, follow: Follow, cap: f32) -> Engine<StdConnect> {
        let endpoint = Endpoint::parse(&format!("ws://127.0.0.1:{port}")).unwrap();
        Engine::new(StdConnect, endpoint, follow, cap)
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
        let mut engine = engine_for(port, Follow::Level, 0.5);

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
        let mut engine = engine_for(port, Follow::Level, 1.0);

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
        let mut engine = engine_for(port, Follow::Beat, 1.0);
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
        let mut engine = engine_for(port, Follow::Level, 0.5);
        assert!(engine.check().is_err());

        engine.audio(&loud());
        assert_eq!(engine.wait, BACKOFF_MIN);
        engine.audio(&loud());
        assert_eq!(engine.wait, BACKOFF_MIN - 1);
    }
}
