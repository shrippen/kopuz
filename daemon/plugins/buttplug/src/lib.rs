//! Drives buttplug.io devices through Intiface Central, following the music.

mod engine;
mod follow;
mod protocol;
mod session;
mod ws;

use std::cell::RefCell;
use std::io;
use std::time::Duration;

use kopuz_plugin_sdk::SourceError;
use kopuz_plugin_sdk::integration::{AudioFrame, Integration, PlaybackEvent, Settings};
use kopuz_plugin_sdk::tcp;

use engine::{Connect, Engine};
use follow::{Follow, Sample};
use ws::{Endpoint, Link};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

impl Link for tcp::Stream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        tcp::Stream::set_read_timeout(self, timeout);
    }
}

struct TcpConnect;

impl Connect for TcpConnect {
    type Link = tcp::Stream;

    fn connect(&self, endpoint: &Endpoint) -> io::Result<tcp::Stream> {
        Ok(tcp::Stream::connect(
            &endpoint.host,
            endpoint.port,
            Some(CONNECT_TIMEOUT),
        )?)
    }
}

pub struct Buttplug {
    url: String,
    /// Nothing when the address is empty or not a `ws://` one.
    engine: RefCell<Option<Engine<TcpConnect>>>,
}

impl Integration for Buttplug {
    fn open(settings: Settings) -> Self {
        let url = settings.get("url").unwrap_or_default().to_string();
        let follow = Follow::from_setting(settings.get("follow"));
        let cap = follow::strength_from_setting(settings.get("strength"));
        let engine = Endpoint::parse(&url)
            .ok()
            .map(|endpoint| Engine::new(TcpConnect, endpoint, follow, cap));
        Buttplug {
            url,
            engine: RefCell::new(engine),
        }
    }

    fn validate(&self) -> Result<(), SourceError> {
        if self.url.is_empty() {
            return Err(SourceError::InvalidInput(
                "Buttplug.io needs the address from Intiface Central".into(),
            ));
        }
        let engine = self.engine.borrow();
        let Some(engine) = engine.as_ref() else {
            let why = Endpoint::parse(&self.url).err().unwrap_or_default();
            return Err(SourceError::InvalidInput(format!(
                "that is not an address Intiface Central uses: {why}, for example ws://127.0.0.1:12345"
            )));
        };
        engine.check().map_err(|why| {
            SourceError::Backend(format!(
                "Intiface Central is not reachable at {}. Start its server and check the \
                 address ({why}).",
                self.url
            ))
        })
    }

    fn on_playback(&self, event: PlaybackEvent) {
        if !matches!(event, PlaybackEvent::Paused(_) | PlaybackEvent::Stopped) {
            return;
        }
        if let Some(engine) = self.engine.borrow_mut().as_mut() {
            engine.stop();
        }
    }

    fn on_audio(&self, frames: Vec<AudioFrame>) {
        let samples: Vec<Sample> = frames
            .iter()
            .map(|frame| Sample {
                level: frame.level,
                low: frame.low,
                beat: frame.beat,
            })
            .collect();
        if let Some(engine) = self.engine.borrow_mut().as_mut() {
            engine.audio(&samples);
        }
    }
}

kopuz_plugin_sdk::export_integration!(Buttplug);
