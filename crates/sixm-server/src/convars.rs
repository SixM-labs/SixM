//! Console variables: named settings shared by the server, the console and scripts.

use std::collections::BTreeMap;
use std::path::PathBuf;

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
        convars.set(IDENTITY_FILE, DEFAULT_IDENTITY_FILE);
        convars.set(TICK_RATE, "20");
        convars.set(ENDPOINT, DEFAULT_ENDPOINT);
        convars.set(MAX_CLIENTS, &DEFAULT_MAX_CLIENTS.to_string());
        convars.set(SERVER_NAME, DEFAULT_SERVER_NAME);
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
