//! An example integration: posts JSON to the address the person gives it.

use kopuz_plugin_sdk::SourceError;
use kopuz_plugin_sdk::http::Client;
use kopuz_plugin_sdk::integration::{
    AudioFrame, Control, ControlKind, Integration, NowPlaying, PlaybackEvent, Section, Settings,
};
use std::cell::Cell;

const PAUSED: &str = "paused";
const TEST: &str = "test";

pub struct Webhook {
    http: Client,
    url: String,
    token: Option<String>,
    beats: bool,
    /// How the last post went; `None` before the first.
    last_ok: Cell<Option<bool>>,
    /// Posts of playback and beats are held back while the person has this on.
    paused: Cell<bool>,
}

impl Webhook {
    fn post(&self, body: &serde_json::Value) -> Result<(), SourceError> {
        let sent = self.send(body);
        self.last_ok.set(Some(sent.is_ok()));
        sent
    }

    fn send(&self, body: &serde_json::Value) -> Result<(), SourceError> {
        let mut request = self.http.post(&self.url).json(body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send()?;
        match response.is_success() {
            true => Ok(()),
            false => Err(SourceError::from_status(response.status(), "the webhook")),
        }
    }
}

impl Integration for Webhook {
    fn open(settings: Settings) -> Self {
        Webhook {
            http: Client::new(),
            url: settings.get("url").unwrap_or_default().to_string(),
            token: settings.get("token").map(str::to_string),
            beats: settings.flag("beats"),
            last_ok: Cell::new(None),
            paused: Cell::new(false),
        }
    }

    fn validate(&self) -> Result<(), SourceError> {
        if self.url.is_empty() {
            return Err(SourceError::InvalidInput(
                "the webhook needs an address".into(),
            ));
        }
        self.post(&serde_json::json!({ "event": "hello" }))
    }

    fn on_playback(&self, event: PlaybackEvent) {
        if self.paused.get() {
            return;
        }
        if let Err(error) = self.post(&body(&event)) {
            kopuz_plugin_sdk::warn!("the webhook was not told: {}", error.message());
        }
    }

    fn on_audio(&self, frames: Vec<AudioFrame>) {
        if !self.beats || self.paused.get() {
            return;
        }
        for frame in frames.iter().filter(|frame| frame.beat) {
            let beat =
                serde_json::json!({ "event": "beat", "level": frame.level, "low": frame.low });
            if let Err(error) = self.post(&beat) {
                kopuz_plugin_sdk::warn!("the webhook was not told of a beat: {}", error.message());
            }
        }
    }

    fn panel(&self) -> Vec<Section> {
        let last = match self.last_ok.get() {
            Some(true) => "ok",
            Some(false) => "failed",
            None => "none yet",
        };
        vec![Section {
            key: String::new(),
            title: "Webhook".into(),
            controls: vec![
                control("last", "Last post", ControlKind::Status, 0.0, last),
                control(
                    PAUSED,
                    "Hold posts",
                    ControlKind::Toggle,
                    self.paused.get().into(),
                    "",
                ),
                control(TEST, "Send test", ControlKind::Button, 0.0, ""),
            ],
        }]
    }

    fn act(
        &self,
        _section: &str,
        control: &str,
        value: f32,
        _text: &str,
    ) -> Result<(), SourceError> {
        match control {
            PAUSED => {
                self.paused.set(value >= 0.5);
                Ok(())
            }
            TEST => self.post(&serde_json::json!({ "event": "test" })),
            _ => Ok(()),
        }
    }
}

fn control(key: &str, label: &str, kind: ControlKind, value: f32, text: &str) -> Control {
    Control {
        key: key.into(),
        label: label.into(),
        kind,
        value,
        text: text.into(),
        quick: false,
    }
}

fn track(track: &NowPlaying) -> serde_json::Value {
    serde_json::json!({
        "key": track.key,
        "title": track.title,
        "artist": track.artist,
        "album": track.album,
        "duration_ms": track.duration_ms,
        "musicbrainz_recording_id": track.musicbrainz_recording_id,
    })
}

fn body(event: &PlaybackEvent) -> serde_json::Value {
    match event {
        PlaybackEvent::Started(started) => serde_json::json!({
            "event": "started",
            "track": track(&started.track),
            "position_ms": started.position_ms,
        }),
        PlaybackEvent::Paused(at) => serde_json::json!({ "event": "paused", "position_ms": at }),
        PlaybackEvent::Resumed(at) => serde_json::json!({ "event": "resumed", "position_ms": at }),
        PlaybackEvent::Seeked(at) => serde_json::json!({ "event": "seeked", "position_ms": at }),
        PlaybackEvent::Stopped => serde_json::json!({ "event": "stopped" }),
    }
}

kopuz_plugin_sdk::export_integration!(Webhook);
