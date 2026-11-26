//! What the panel shows: the plugin's own section and one per device.

use kopuz_plugin_sdk::integration::{ChoiceOption, Control, ControlKind, Section, Slider};

use crate::follow::Follow;
use crate::prefs::{
    ACTIVE, FOLLOW, FOLLOWS, LIKE_REST, NOW, ON, PERCENT, Prefs, SCAN, STATUS, STRENGTH, TEST,
};
use crate::session::DeviceState;

/// Sliders move in steps of this many percent.
const SLIDER_STEP: f32 = 5.0;

/// How the link to Intiface Central stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connected,
    Connecting,
    Unreachable,
}

impl Link {
    fn text(self) -> &'static str {
        match self {
            Link::Connected => "Connected",
            Link::Connecting => "Connecting…",
            Link::Unreachable => "Not reachable — start its server",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quick {
    Yes,
    No,
}

fn control(key: &str, label: &str, kind: ControlKind, quick: Quick) -> Control {
    Control {
        key: key.into(),
        label: label.into(),
        kind,
        value: 0.0,
        text: String::new(),
        quick: quick == Quick::Yes,
    }
}

fn status(text: &str) -> Control {
    Control {
        text: text.into(),
        ..control(STATUS, "Status", ControlKind::Status, Quick::No)
    }
}

fn toggle(key: &str, label: &str, on: bool, quick: Quick) -> Control {
    Control {
        value: on.into(),
        ..control(key, label, ControlKind::Toggle, quick)
    }
}

fn slider(key: &str, label: &str, fraction: f32, quick: Quick) -> Control {
    let kind = ControlKind::Slider(Slider {
        min: 0.0,
        max: PERCENT,
        step: SLIDER_STEP,
    });
    Control {
        value: fraction * PERCENT,
        ..control(key, label, kind, quick)
    }
}

/// A choice of what to follow; `rest` adds "Like the rest" first.
fn choice(key: &str, label: &str, picked: &str, rest: bool, quick: Quick) -> Control {
    let mut options = Vec::new();
    if rest {
        options.push(ChoiceOption {
            value: LIKE_REST.into(),
            label: "Like the rest".into(),
        });
    }
    options.extend(Follow::ALL.map(|follow| ChoiceOption {
        value: follow.key().into(),
        label: follow.label().into(),
    }));
    Control {
        text: picked.into(),
        ..control(key, label, ControlKind::Choice(options), quick)
    }
}

fn plugin_section(link: Link, prefs: &Prefs) -> Section {
    Section {
        key: String::new(),
        title: "Buttplug.io".into(),
        controls: vec![
            Control {
                label: "Intiface Central".into(),
                ..status(link.text())
            },
            toggle(ACTIVE, "Active", prefs.active, Quick::Yes),
            slider(STRENGTH, "Strength", prefs.master, Quick::Yes),
            choice(FOLLOW, "Follow", prefs.follow.key(), false, Quick::Yes),
            control(SCAN, "Scan for devices", ControlKind::Button, Quick::No),
        ],
    }
}

fn device_section(key: &str, title: &str, now: Option<f32>, prefs: &Prefs) -> Section {
    let controls = match now {
        None => vec![status("Not supported yet")],
        Some(now) => {
            let device = prefs.device(key);
            let follows = device.follow.map_or(LIKE_REST, Follow::key);
            vec![
                Control {
                    value: now,
                    ..control(NOW, "Now", ControlKind::Meter, Quick::Yes)
                },
                toggle(ON, "On", device.on, Quick::No),
                slider(STRENGTH, "Strength", device.cap, Quick::No),
                choice(FOLLOWS, "Follows", follows, true, Quick::No),
                control(TEST, "Test", ControlKind::Button, Quick::No),
            ]
        }
    };
    Section {
        key: key.into(),
        title: title.into(),
        controls,
    }
}

/// The plugin's section, then one per device Intiface knows.
pub fn sections(link: Link, prefs: &Prefs, state: Option<&DeviceState>) -> Vec<Section> {
    let mut sections = vec![plugin_section(link, prefs)];
    let Some(state) = state else {
        return sections;
    };

    let keys = state.keys();
    for device in state.devices.values() {
        let now = device.supported().then(|| state.now(device));
        sections.push(device_section(
            &keys[&device.index],
            &device.name,
            now,
            prefs,
        ));
    }
    sections
}
