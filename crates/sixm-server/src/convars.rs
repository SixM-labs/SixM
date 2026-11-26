//! Console variables: named settings shared by the server, the console and scripts.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Convar naming the folder that is scanned for resources.
const RESOURCES_PATH: &str = "sixm_resourcesPath";
/// Convar naming the frame rate of the main loop.
const TICK_RATE: &str = "sixm_tickRate";

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
        convars.set(RESOURCES_PATH, "resources");
        convars.set(TICK_RATE, "20");
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

    /// The folder to scan for resources. Relative paths are relative to the working directory.
    pub(crate) fn resources_path(&self) -> PathBuf {
        PathBuf::from(self.get(RESOURCES_PATH).unwrap_or("resources"))
    }

    /// Frames per second, from the tick-rate convar. Clamped to `1..=1000`;
    /// a value that is not a number means the default of 20.
    pub(crate) fn tick_rate(&self) -> u64 {
        self.get(TICK_RATE)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .map_or(20, |rate| rate.clamp(1, 1000).unsigned_abs())
    }
}
