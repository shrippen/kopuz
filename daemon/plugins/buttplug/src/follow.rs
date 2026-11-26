//! Turns what the music sounds like into one drive value from 0 to 1.

/// What the plugin keeps of an audio frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub level: f32,
    pub low: f32,
    pub beat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    Level,
    Low,
    Beat,
}

impl Follow {
    pub const ALL: [Follow; 3] = [Follow::Level, Follow::Low, Follow::Beat];

    /// What a choice control carries for it.
    pub fn key(self) -> &'static str {
        match self {
            Follow::Level => "level",
            Follow::Low => "low",
            Follow::Beat => "beat",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Follow::Level => "The loudness",
            Follow::Low => "The bass",
            Follow::Beat => "The beat",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Follow::ALL.into_iter().find(|follow| follow.key() == key)
    }
}

/// An RMS of music rarely passes 0.3, so it is scaled up to fill the range.
const LEVEL_GAIN: f32 = 3.0;
/// Share of the gap to a louder frame closed per frame: fast attack.
const ATTACK: f32 = 0.6;
/// Share of the gap to a quieter frame closed per frame: about 250 ms to fade.
const RELEASE: f32 = 0.08;
/// A beat's pulse falls to nothing over about 250 ms.
const BEAT_DECAY: f32 = 0.08;

pub struct Follower {
    follow: Follow,
    cap: f32,
    value: f32,
}

impl Follower {
    pub fn new(follow: Follow, cap: f32) -> Self {
        Follower {
            follow,
            cap: cap.clamp(0.0, 1.0),
            value: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.value = 0.0;
    }

    fn step(&mut self, sample: &Sample) {
        let input = match self.follow {
            Follow::Level => (sample.level * LEVEL_GAIN).clamp(0.0, 1.0),
            Follow::Low => (sample.low * LEVEL_GAIN).clamp(0.0, 1.0),
            Follow::Beat => {
                self.value = if sample.beat {
                    1.0
                } else {
                    (self.value - BEAT_DECAY).max(0.0)
                };
                return;
            }
        };
        let rate = if input > self.value { ATTACK } else { RELEASE };
        self.value += (input - self.value) * rate;
    }

    /// Takes the frames since the last call; returns the capped drive. No
    /// frames count as one silent one, so the value still fades.
    pub fn feed(&mut self, samples: &[Sample]) -> f32 {
        match samples.is_empty() {
            true => self.step(&Sample::default()),
            false => samples.iter().for_each(|sample| self.step(sample)),
        }
        (self.value * self.cap).clamp(0.0, 1.0)
    }
}

/// One follower per mode, so each device can follow its own.
pub struct Followers {
    level: Follower,
    low: Follower,
    beat: Follower,
}

/// What each mode drives at, from 0 to 1.
#[derive(Debug, Clone, Copy, Default)]
pub struct Levels {
    level: f32,
    low: f32,
    beat: f32,
    /// Whether a beat came in the frames just fed.
    hit: bool,
}

impl Levels {
    pub fn hit(&self) -> bool {
        self.hit
    }

    pub fn get(&self, follow: Follow) -> f32 {
        match follow {
            Follow::Level => self.level,
            Follow::Low => self.low,
            Follow::Beat => self.beat,
        }
    }
}

impl Default for Followers {
    fn default() -> Self {
        Followers {
            level: Follower::new(Follow::Level, 1.0),
            low: Follower::new(Follow::Low, 1.0),
            beat: Follower::new(Follow::Beat, 1.0),
        }
    }
}

impl Followers {
    pub fn feed(&mut self, samples: &[Sample]) -> Levels {
        Levels {
            level: self.level.feed(samples),
            low: self.low.feed(samples),
            beat: self.beat.feed(samples),
            hit: samples.iter().any(|sample| sample.beat),
        }
    }

    pub fn reset(&mut self) {
        self.level.reset();
        self.low.reset();
        self.beat.reset();
    }
}

/// `value` as whole steps of a device with `step_count` steps.
pub fn quantize(value: f32, step_count: u32) -> u32 {
    (value.clamp(0.0, 1.0) * step_count as f32).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loud(level: f32) -> Sample {
        Sample {
            level,
            low: level,
            beat: false,
        }
    }

    #[test]
    fn the_level_rises_fast_and_falls_slowly() {
        let mut follower = Follower::new(Follow::Level, 1.0);
        let up = follower.feed(&[loud(0.3), loud(0.3)]);
        assert!(up > 0.7, "attack was {up}");

        let down = follower.feed(&[loud(0.0)]);
        assert!(down > 0.7 * up, "release was {down}");
        assert!(follower.feed(&[]) < down);
    }

    #[test]
    fn the_bass_ignores_the_overall_level() {
        let mut follower = Follower::new(Follow::Low, 1.0);
        let quiet_bass = Sample {
            level: 0.9,
            low: 0.0,
            beat: false,
        };
        assert_eq!(follower.feed(&[quiet_bass]), 0.0);
        assert!(follower.feed(&[loud(0.3)]) > 0.5);
    }

    #[test]
    fn a_beat_pulses_to_full_and_fades_in_about_250_ms() {
        let mut follower = Follower::new(Follow::Beat, 1.0);
        let beat = Sample {
            level: 0.0,
            low: 0.0,
            beat: true,
        };
        assert_eq!(follower.feed(&[beat]), 1.0);

        let mut frames = 0;
        while follower.feed(&[Sample::default()]) > 0.0 {
            frames += 1;
        }
        assert!((10..=14).contains(&frames), "faded after {frames} frames");
    }

    #[test]
    fn the_strength_caps_the_output() {
        let mut follower = Follower::new(Follow::Beat, 0.25);
        let beat = Sample {
            beat: true,
            ..Sample::default()
        };
        assert_eq!(follower.feed(&[beat]), 0.25);
    }

    #[test]
    fn a_follow_mode_round_trips_through_its_key() {
        for follow in Follow::ALL {
            assert_eq!(Follow::from_key(follow.key()), Some(follow));
        }
        assert_eq!(Follow::from_key(""), None);
        assert_eq!(Follow::from_key("bogus"), None);
    }

    #[test]
    fn every_mode_follows_at_once() {
        let mut followers = Followers::default();
        let beat = Sample {
            level: 0.0,
            low: 0.3,
            beat: true,
        };
        let levels = followers.feed(&[beat]);
        assert_eq!(levels.get(Follow::Beat), 1.0);
        assert!(levels.hit());
        assert!(!followers.feed(&[]).hit());
        assert_eq!(levels.get(Follow::Level), 0.0);
        assert!(levels.get(Follow::Low) > 0.4);
        followers.reset();
        assert_eq!(followers.feed(&[]).get(Follow::Beat), 0.0);
    }

    #[test]
    fn values_snap_to_a_devices_steps() {
        assert_eq!(quantize(0.5, 10), 5);
        assert_eq!(quantize(0.54, 10), 5);
        assert_eq!(quantize(0.56, 10), 6);
        assert_eq!(quantize(0.04, 10), 0);
        assert_eq!(quantize(2.0, 20), 20);
        assert_eq!(quantize(-1.0, 20), 0);
    }
}
