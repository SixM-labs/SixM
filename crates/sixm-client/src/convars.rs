//! Console variables of the client: named settings that `set` changes, and
//! convars the server replicates. Scripts read both with `GetConvar`.

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
    /// Convars this client sets itself.
    values: BTreeMap<String, String>,
    /// Convars the server replicates. They take precedence over `values`.
    replicated: BTreeMap<String, String>,
}

impl Convars {
    /// A store holding the built-in defaults.
    pub(crate) fn new() -> Self {
        let mut convars = Self {
            values: BTreeMap::new(),
            replicated: BTreeMap::new(),
        };
        convars.set(TICK_RATE, &DEFAULT_TICK_RATE.to_string());
        convars
    }

    /// The value of `name`: the replicated value when the server sends one,
    /// otherwise the local value, if it is set.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        let key = name.to_ascii_lowercase();
        self.replicated
            .get(&key)
            .or_else(|| self.values.get(&key))
            .map(String::as_str)
    }

    /// Whether the server replicates `name`. Such a convar cannot be set here.
    pub(crate) fn is_replicated(&self, name: &str) -> bool {
        self.replicated.contains_key(&name.to_ascii_lowercase())
    }

    /// Sets the local value of `name` to `value`, replacing any previous value.
    pub(crate) fn set(&mut self, name: &str, value: &str) {
        self.values
            .insert(name.to_ascii_lowercase(), value.to_owned());
    }

    /// Applies the convars the server sent, as `(name, value)`. A `None` value
    /// means the server no longer replicates the convar, so the client forgets
    /// it and reads the local value again. Returns `(key, effective_value, replicated)`
    /// for each convar whose effective value changed.
    pub(crate) fn apply_replicated(
        &mut self,
        entries: Vec<(String, Option<String>)>,
    ) -> Vec<(String, Option<String>, bool)> {
        let mut changed = Vec::new();
        for (name, value) in entries {
            let key = name.to_ascii_lowercase();
            let prev = self.get(&key).map(str::to_owned);
            let is_replicated = value.is_some();
            match value {
                Some(value) => {
                    self.replicated.insert(key.clone(), value);
                }
                None => {
                    self.replicated.remove(&key);
                }
            }
            let next = self.get(&key).map(str::to_owned);
            if prev != next {
                changed.push((key, next, is_replicated));
            }
        }
        changed
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicated_values_come_before_local_ones() {
        let mut convars = Convars::new();
        assert_eq!(convars.get("game_mode"), None);

        convars.set("game_mode", "local");
        assert_eq!(convars.get("game_mode"), Some("local"));

        convars.apply_replicated(vec![("Game_Mode".to_owned(), Some("race".to_owned()))]);
        assert_eq!(convars.get("GAME_MODE"), Some("race"));
        assert!(convars.is_replicated("game_mode"));

        convars.apply_replicated(vec![("game_mode".to_owned(), None)]);
        assert_eq!(convars.get("game_mode"), Some("local"));
        assert!(!convars.is_replicated("game_mode"));
    }
}
