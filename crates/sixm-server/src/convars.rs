//! Console variables: named settings shared by the server, the console and scripts.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use sixm_core::ConvarKind;
use sixm_net::Identity;

/// Convar naming the folder that is scanned for resources.
const RESOURCES_PATH: &str = "sixm_resourcesPath";
/// Convar naming the file that keeps the server's certificate. Empty means a
/// new certificate on every start.
const IDENTITY_FILE: &str = "sixm_identityFile";
/// Convar naming the frame rate of the main loop.
const TICK_RATE: &str = "sixm_tickRate";
/// Convar naming the address the server listens on, e.g. `0.0.0.0:30120`.
pub(crate) const ENDPOINT: &str = "endpoint_add_udp";
/// Convar naming the most players accepted at once.
const MAX_CLIENTS: &str = "sv_maxclients";
/// Convar naming the server, shown to players when they connect.
const SERVER_NAME: &str = "sv_hostname";
/// Convar naming how many seconds a deferred connection may wait for `done`.
const DEFERRAL_TIMEOUT: &str = "sixm_deferralTimeout";
/// Convar naming the model of the ped created for each player.
const PLAYER_MODEL: &str = "sixm_playerModel";

/// Certificate file when `sixm_identityFile` is not set.
const DEFAULT_IDENTITY_FILE: &str = "cache/server-identity.bin";
/// Listen address when `endpoint_add_udp` is not set.
const DEFAULT_ENDPOINT: &str = "0.0.0.0:30120";
/// Player limit when `sv_maxclients` is not set.
const DEFAULT_MAX_CLIENTS: i64 = 32;
/// Smallest and largest player limit the server honours.
const MIN_CLIENTS: i64 = 1;
const MAX_CLIENTS_LIMIT: i64 = 1024;
/// Server name when `sv_hostname` is not set.
const DEFAULT_SERVER_NAME: &str = "SixM Server";
/// Deferral timeout in seconds when `sixm_deferralTimeout` is not set.
const DEFAULT_DEFERRAL_TIMEOUT: i64 = 120;
/// Model name of a player's ped when `sixm_playerModel` is not set.
const DEFAULT_PLAYER_MODEL: &str = "player";
/// Smallest and largest deferral timeout in seconds the server honours.
const MIN_DEFERRAL_TIMEOUT: i64 = 1;
const MAX_DEFERRAL_TIMEOUT: i64 = 3600;

/// A stored convar: its value and the kind the last store gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    value: String,
    kind: ConvarKind,
}

/// The convar store. Names are case-insensitive and kept in lower case.
#[derive(Debug)]
pub(crate) struct Convars {
    values: BTreeMap<String, Entry>,
}

impl Convars {
    /// A store holding the built-in defaults, all plain.
    pub(crate) fn new() -> Self {
        let mut convars = Self {
            values: BTreeMap::new(),
        };
        convars.set(RESOURCES_PATH, "resources", ConvarKind::Plain);
        convars.set(IDENTITY_FILE, DEFAULT_IDENTITY_FILE, ConvarKind::Plain);
        convars.set(TICK_RATE, "20", ConvarKind::Plain);
        convars.set(ENDPOINT, DEFAULT_ENDPOINT, ConvarKind::Plain);
        convars.set(
            MAX_CLIENTS,
            &DEFAULT_MAX_CLIENTS.to_string(),
            ConvarKind::Plain,
        );
        convars.set(SERVER_NAME, DEFAULT_SERVER_NAME, ConvarKind::Plain);
        convars.set(
            DEFERRAL_TIMEOUT,
            &DEFAULT_DEFERRAL_TIMEOUT.to_string(),
            ConvarKind::Plain,
        );
        convars.set(PLAYER_MODEL, DEFAULT_PLAYER_MODEL, ConvarKind::Plain);
        convars
    }

    /// The value of `name`, if it is set.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(|entry| entry.value.as_str())
    }

    /// The kind of `name`, if it is set.
    pub(crate) fn kind(&self, name: &str) -> Option<ConvarKind> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(|entry| entry.kind)
    }

    /// Stores `value` under `name` with `kind`, replacing any previous entry.
    /// Returns the kind the convar had before, or `None` when it was not set.
    pub(crate) fn set(&mut self, name: &str, value: &str, kind: ConvarKind) -> Option<ConvarKind> {
        self.values
            .insert(
                name.to_ascii_lowercase(),
                Entry {
                    value: value.to_owned(),
                    kind,
                },
            )
            .map(|previous| previous.kind)
    }

    /// Every replicated convar as `(name, value)`, in name order.
    pub(crate) fn replicated(&self) -> Vec<(String, String)> {
        self.values
            .iter()
            .filter(|(_, entry)| entry.kind == ConvarKind::Replicated)
            .map(|(name, entry)| (name.clone(), entry.value.clone()))
            .collect()
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

    /// Where the server's certificate comes from: the file named by
    /// `sixm_identityFile`, or a fresh certificate when the convar is empty.
    pub(crate) fn identity(&self) -> Identity {
        let path = self
            .get(IDENTITY_FILE)
            .unwrap_or(DEFAULT_IDENTITY_FILE)
            .trim();
        if path.is_empty() {
            Identity::Ephemeral
        } else {
            Identity::Persistent(PathBuf::from(path))
        }
    }

    /// The address to listen on, as written. It is parsed when the network starts.
    pub(crate) fn endpoint(&self) -> &str {
        self.get(ENDPOINT).unwrap_or(DEFAULT_ENDPOINT)
    }

    /// The name sent to players in the welcome message.
    pub(crate) fn server_name(&self) -> &str {
        self.get(SERVER_NAME).unwrap_or(DEFAULT_SERVER_NAME)
    }

    /// How long a connection that `playerConnecting` deferred may wait for
    /// `done`. Clamped to `1..=3600` seconds; a value that is not a number
    /// means the default of 120.
    pub(crate) fn deferral_timeout(&self) -> Duration {
        let seconds = self
            .get(DEFERRAL_TIMEOUT)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_DEFERRAL_TIMEOUT);
        Duration::from_secs(
            seconds
                .clamp(MIN_DEFERRAL_TIMEOUT, MAX_DEFERRAL_TIMEOUT)
                .unsigned_abs(),
        )
    }

    /// The model name of each player's ped, from `sixm_playerModel`.
    pub(crate) fn player_model(&self) -> &str {
        self.get(PLAYER_MODEL).unwrap_or(DEFAULT_PLAYER_MODEL)
    }

    /// The most players accepted at once. Clamped to `1..=1024`; a value that
    /// is not a number means the default of 32.
    pub(crate) fn max_clients(&self) -> usize {
        let value = self
            .get(MAX_CLIENTS)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_MAX_CLIENTS);
        usize::try_from(value.clamp(MIN_CLIENTS, MAX_CLIENTS_LIMIT)).unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_convars_are_plain() {
        let convars = Convars::new();
        assert_eq!(convars.kind(SERVER_NAME), Some(ConvarKind::Plain));
        assert_eq!(convars.replicated(), Vec::new());
    }

    #[test]
    fn the_last_store_decides_the_kind() {
        let mut convars = Convars::new();
        assert_eq!(convars.kind("game_mode"), None);

        let before = convars.set("Game_Mode", "race", ConvarKind::Replicated);
        assert_eq!(before, None);
        assert_eq!(convars.kind("game_mode"), Some(ConvarKind::Replicated));

        let before = convars.set("game_mode", "info", ConvarKind::ServerInfo);
        assert_eq!(before, Some(ConvarKind::Replicated));
        assert_eq!(convars.kind("GAME_MODE"), Some(ConvarKind::ServerInfo));

        let before = convars.set("game_mode", "plain", ConvarKind::Plain);
        assert_eq!(before, Some(ConvarKind::ServerInfo));
        assert_eq!(convars.get("GAME_MODE"), Some("plain"));
        assert_eq!(convars.kind("game_mode"), Some(ConvarKind::Plain));
    }

    #[test]
    fn only_replicated_convars_are_listed_with_their_latest_value() {
        let mut convars = Convars::new();
        convars.set("a", "1", ConvarKind::Plain);
        convars.set("b", "2", ConvarKind::ServerInfo);
        convars.set("d", "4", ConvarKind::Replicated);
        convars.set("c", "3", ConvarKind::Replicated);
        convars.set("c", "5", ConvarKind::Replicated);
        assert_eq!(
            convars.replicated(),
            [
                ("c".to_owned(), "5".to_owned()),
                ("d".to_owned(), "4".to_owned()),
            ]
        );

        convars.set("c", "gone", ConvarKind::Plain);
        assert_eq!(convars.replicated(), [("d".to_owned(), "4".to_owned())]);
    }
}
