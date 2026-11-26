//! Console variables of the client: named settings that `set` changes and
//! scripts read with `GetConvar`.

use std::collections::BTreeMap;

/// Convar naming the frame rate of the main loop.
const TICK_RATE: &str = "sixm_tickRate";
/// Frames per second when `sixm_tickRate` is not set, or is not a number.
const DEFAULT_TICK_RATE: u64 = 60;
/// Smallest and largest frame rate the client honours.
const MIN_TICK_RATE: i64 = 1;
const MAX_TICK_RATE: i64 = 1000;

/// The convar store. Names are case-insensitive and kept in lower case.
#[derive(Debug)]
pub(crate) struct Convars {
    values: BTreeMap<String, String>,
}

impl Convars {
    /// A store holding the built-in defaults.
    pub(crate) fn new() -> Self {
        let mut convars = Self {
            values: BTreeMap::new(),
        };
        convars.set(TICK_RATE, &DEFAULT_TICK_RATE.to_string());
        convars
    }

    /// The value of `name`, if it is set.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Sets `name` to `value`, replacing any previous value.
    pub(crate) fn set(&mut self, name: &str, value: &str) {
        self.values
            .insert(name.to_ascii_lowercase(), value.to_owned());
    }

    /// Frames per second, clamped to `1..=1000`.
    pub(crate) fn tick_rate(&self) -> u64 {
        self.get(TICK_RATE)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .map_or(DEFAULT_TICK_RATE, |rate| {
                rate.clamp(MIN_TICK_RATE, MAX_TICK_RATE).unsigned_abs()
            })
    }
}
