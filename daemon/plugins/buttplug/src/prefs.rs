//! What the person set in the panel: the master switch and strength, what to
//! follow, and the same per device. Devices are known only once connected, but
//! the host replays settings right after opening, so they are kept by section
//! key whether or not that device is present.

use std::collections::BTreeMap;

use kopuz_plugin_sdk::SourceError;

use crate::follow::Follow;

/// Control keys of the plugin's own section (key "").
pub const ACTIVE: &str = "active";
pub const STRENGTH: &str = "strength";
pub const FOLLOW: &str = "follow";
pub const SCAN: &str = "scan";
pub const STATUS: &str = "status";

/// Control keys of a device's section; it shares `STRENGTH` and `STATUS`.
pub const NOW: &str = "now";
pub const ON: &str = "on";
pub const FOLLOWS: &str = "follows";
pub const TEST: &str = "test";

pub const DEFAULT_MASTER: f32 = 0.5;
/// Sliders are in percent.
pub const PERCENT: f32 = 100.0;
/// The choice value of "Like the rest".
pub const LIKE_REST: &str = "";

/// What the engine does after a setting changed.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    StopAll,
    /// The device's section key.
    StopDevice(String),
    Scan,
    Test(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DevicePrefs {
    pub on: bool,
    /// A cap from 0 to 1, on top of the master.
    pub cap: f32,
    /// `None` follows the plugin's choice.
    pub follow: Option<Follow>,
}

impl Default for DevicePrefs {
    fn default() -> Self {
        DevicePrefs {
            on: true,
            cap: 1.0,
            follow: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Prefs {
    pub active: bool,
    pub master: f32,
    pub follow: Follow,
    devices: BTreeMap<String, DevicePrefs>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            active: true,
            master: DEFAULT_MASTER,
            follow: Follow::Level,
            devices: BTreeMap::new(),
        }
    }
}

fn invalid(why: &str) -> SourceError {
    SourceError::InvalidInput(why.to_string())
}

/// A slider's percent as a fraction.
fn fraction(value: f32) -> Result<f32, SourceError> {
    if !(0.0..=PERCENT).contains(&value) {
        return Err(invalid("a strength is from 0 to 100"));
    }
    Ok(value / PERCENT)
}

fn follow_of(text: &str) -> Result<Follow, SourceError> {
    Follow::from_key(text).ok_or_else(|| invalid("that is not something to follow"))
}

impl Prefs {
    /// A device's settings; the defaults for one never set.
    pub fn device(&self, key: &str) -> DevicePrefs {
        self.devices.get(key).copied().unwrap_or_default()
    }

    /// What a device follows: its own choice, else the plugin's.
    pub fn follows(&self, key: &str) -> Follow {
        self.device(key).follow.unwrap_or(self.follow)
    }

    /// Takes a change from the panel. Any section but the plugin's own is
    /// taken as a device's, present or not.
    pub fn act(
        &mut self,
        section: &str,
        control: &str,
        value: f32,
        text: &str,
    ) -> Result<Action, SourceError> {
        match section.is_empty() {
            true => self.act_plugin(control, value, text),
            false => self.act_device(section, control, value, text),
        }
    }

    fn act_plugin(&mut self, control: &str, value: f32, text: &str) -> Result<Action, SourceError> {
        match control {
            ACTIVE => {
                self.active = value >= 0.5;
                Ok(match self.active {
                    true => Action::Nothing,
                    false => Action::StopAll,
                })
            }
            STRENGTH => {
                self.master = fraction(value)?;
                Ok(Action::Nothing)
            }
            FOLLOW => {
                self.follow = follow_of(text)?;
                Ok(Action::Nothing)
            }
            SCAN => Ok(Action::Scan),
            _ => Err(invalid("there is no such control")),
        }
    }

    fn act_device(
        &mut self,
        section: &str,
        control: &str,
        value: f32,
        text: &str,
    ) -> Result<Action, SourceError> {
        if !matches!(control, ON | STRENGTH | FOLLOWS | TEST) {
            return Err(invalid("there is no such control"));
        }
        if control == TEST {
            return Ok(Action::Test(section.to_string()));
        }

        let device = self.devices.entry(section.to_string()).or_default();
        match control {
            ON => {
                device.on = value >= 0.5;
                if !device.on {
                    return Ok(Action::StopDevice(section.to_string()));
                }
            }
            STRENGTH => device.cap = fraction(value)?,
            _ => {
                device.follow = match text {
                    LIKE_REST => None,
                    _ => Some(follow_of(text)?),
                }
            }
        }
        Ok(Action::Nothing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_half_strength_and_the_loudness() {
        let prefs = Prefs::default();
        assert!(prefs.active);
        assert_eq!(prefs.master, 0.5);
        assert_eq!(prefs.follows("any"), Follow::Level);
        assert_eq!(prefs.device("any"), DevicePrefs::default());
    }

    #[test]
    fn a_device_setting_is_kept_before_the_device_exists() {
        let mut prefs = Prefs::default();
        prefs.act("Toy", STRENGTH, 40.0, "").unwrap();
        prefs.act("Toy", FOLLOWS, 0.0, "beat").unwrap();
        assert_eq!(prefs.device("Toy").cap, 0.4);
        assert_eq!(prefs.follows("Toy"), Follow::Beat);
        assert_eq!(prefs.follows("Other"), Follow::Level);

        prefs.act("Toy", FOLLOWS, 0.0, LIKE_REST).unwrap();
        assert_eq!(prefs.follows("Toy"), Follow::Level);
    }

    #[test]
    fn switching_off_asks_for_a_stop() {
        let mut prefs = Prefs::default();
        assert_eq!(prefs.act("", ACTIVE, 0.0, "").unwrap(), Action::StopAll);
        assert_eq!(
            prefs.act("Toy", ON, 0.0, "").unwrap(),
            Action::StopDevice("Toy".into())
        );
        assert_eq!(prefs.act("Toy", ON, 1.0, "").unwrap(), Action::Nothing);
        assert_eq!(prefs.act("", SCAN, 0.0, "").unwrap(), Action::Scan);
        assert_eq!(
            prefs.act("Toy", TEST, 0.0, "").unwrap(),
            Action::Test("Toy".into())
        );
    }

    #[test]
    fn bad_values_are_refused_and_change_nothing() {
        let mut prefs = Prefs::default();
        for (section, control, value, text) in [
            ("", STRENGTH, 101.0, ""),
            ("", STRENGTH, -1.0, ""),
            ("", STRENGTH, f32::NAN, ""),
            ("", FOLLOW, 0.0, "bogus"),
            ("", FOLLOW, 0.0, LIKE_REST),
            ("", "nope", 0.0, ""),
            ("", NOW, 0.0, ""),
            ("Toy", STRENGTH, 120.0, ""),
            ("Toy", FOLLOWS, 0.0, "bogus"),
            ("Toy", "nope", 0.0, ""),
            ("Toy", STATUS, 0.0, ""),
        ] {
            let got = prefs.act(section, control, value, text);
            assert!(
                matches!(got, Err(SourceError::InvalidInput(_))),
                "{section}/{control}: {got:?}"
            );
        }
        assert_eq!(prefs.master, DEFAULT_MASTER);
        assert_eq!(prefs.device("Toy"), DevicePrefs::default());
    }
}
