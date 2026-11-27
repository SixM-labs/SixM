//! Console variables: named settings shared by the server, the console and scripts.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use ring::digest::{Digest, SHA256, digest};
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
/// Whether `/players.json` lists identifiers. Off unless set to `true`.
const EXPOSE_IDENTIFIERS: &str = "sv_exposePlayerIdentifiers";
/// Password protecting `POST /rcon`. Unset or empty disables remote console access.
pub(crate) const RCON_PASSWORD: &str = "rcon_password";
/// Whether the HTTP listener runs. On unless set to `false` or `0`.
const HTTP_ENABLED: &str = "sv_httpEnabled";
/// Convar naming how many seconds a resource's HTTP handler may take to
/// complete its response.
const HTTP_HANDLER_TIMEOUT: &str = "sixm_httpHandlerTimeout";
/// Convar naming how many seconds a deferred connection may wait for `done`.
const DEFERRAL_TIMEOUT: &str = "sixm_deferralTimeout";
/// Convar naming how many seconds an HTTP request from `PerformHttpRequest`
/// may take, unless its options give another timeout.
const HTTP_TIMEOUT: &str = "sixm_httpTimeout";
/// Convar naming how many threads run the HTTP requests of
/// `PerformHttpRequest`. Read when the first request starts.
const HTTP_WORKERS: &str = "sixm_httpWorkers";
/// Convar naming the model of the ped created for each player.
const PLAYER_MODEL: &str = "sixm_playerModel";
/// Convar naming the distance in metres within which a player is sent
/// entities. Stored in lower case, as the store keeps every name, so that
/// reading it does not allocate.
const CULL_RADIUS: &str = "sixm_cullradius";

/// Most entities one client may create at once. Replicated, so the client
/// refuses at the same limit without asking.
const MAX_CLIENT_ENTITIES: &str = "sixm_maxClientEntities";
/// The limit when `sixm_maxClientEntities` is not set, and the largest one.
const DEFAULT_MAX_CLIENT_ENTITIES: i64 = 64;
const MAX_CLIENT_ENTITIES_LIMIT: i64 = 4096;
/// Global entity creation lockdown mode (`inactive`, `relaxed`, `strict`),
/// overridable per routing bucket.
const ENTITY_LOCKDOWN: &str = "sv_entityLockdown";
const DEFAULT_ENTITY_LOCKDOWN: &str = "inactive";

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
/// Handler timeout in seconds when `sixm_httpHandlerTimeout` is not set, and
/// the clamp. The listener's own limit in `sixm-net` is above the largest value.
const DEFAULT_HTTP_HANDLER_TIMEOUT: i64 = 25;
const MIN_HTTP_HANDLER_TIMEOUT: i64 = 1;
const MAX_HTTP_HANDLER_TIMEOUT: i64 = 120;
/// HTTP request timeout in seconds when `sixm_httpTimeout` is not set.
const DEFAULT_HTTP_TIMEOUT: i64 = 30;
/// Smallest and largest HTTP timeout in seconds the server honours.
const MIN_HTTP_TIMEOUT: i64 = 1;
const MAX_HTTP_TIMEOUT: i64 = 600;
/// HTTP worker threads when `sixm_httpWorkers` is not set, and the clamp.
const DEFAULT_HTTP_WORKERS: i64 = 8;
const MIN_HTTP_WORKERS: i64 = 1;
const MAX_HTTP_WORKERS: i64 = 64;
/// Model name of a player's ped when `sixm_playerModel` is not set.
const DEFAULT_PLAYER_MODEL: &str = "player";
/// Scoping radius in metres when `sixm_cullRadius` is not set, and the largest
/// the server honours.
const DEFAULT_CULL_RADIUS: f32 = 424.0;
const MAX_CULL_RADIUS: f32 = 100_000.0;
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
    rcon_password: Option<Digest>,
}

impl Convars {
    /// A store holding the built-in defaults, all plain.
    pub(crate) fn new() -> Self {
        let mut convars = Self {
            values: BTreeMap::new(),
            rcon_password: None,
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
        convars.set(
            HTTP_TIMEOUT,
            &DEFAULT_HTTP_TIMEOUT.to_string(),
            ConvarKind::Plain,
        );
        convars.set(
            HTTP_WORKERS,
            &DEFAULT_HTTP_WORKERS.to_string(),
            ConvarKind::Plain,
        );
        convars.set(PLAYER_MODEL, DEFAULT_PLAYER_MODEL, ConvarKind::Plain);
        convars.set(
            CULL_RADIUS,
            &DEFAULT_CULL_RADIUS.to_string(),
            ConvarKind::Plain,
        );
        convars.set(
            MAX_CLIENT_ENTITIES,
            &DEFAULT_MAX_CLIENT_ENTITIES.to_string(),
            ConvarKind::Replicated,
        );
        convars.set(ENTITY_LOCKDOWN, DEFAULT_ENTITY_LOCKDOWN, ConvarKind::Plain);
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

    /// Every server-info convar as `(name, value)`, in name order.
    pub(crate) fn server_info(&self) -> Vec<(String, String)> {
        self.values
            .iter()
            .filter(|(_, entry)| entry.kind == ConvarKind::ServerInfo)
            .map(|(name, entry)| (name.clone(), entry.value.clone()))
            .collect()
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

    /// The default timeout of an HTTP request, from `sixm_httpTimeout`. Clamped
    /// to `1` to `600` seconds; a value that is not a number means 30.
    pub(crate) fn http_timeout(&self) -> Duration {
        let seconds = self
            .get(HTTP_TIMEOUT)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_HTTP_TIMEOUT);
        Duration::from_secs(
            seconds
                .clamp(MIN_HTTP_TIMEOUT, MAX_HTTP_TIMEOUT)
                .unsigned_abs(),
        )
    }

    /// The number of HTTP worker threads, from `sixm_httpWorkers`. Clamped to
    /// `1` to `64`; a value that is not a number means 8.
    pub(crate) fn http_workers(&self) -> usize {
        let workers = self
            .get(HTTP_WORKERS)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_HTTP_WORKERS);
        usize::try_from(workers.clamp(MIN_HTTP_WORKERS, MAX_HTTP_WORKERS)).unwrap_or(1)
    }

    /// Whether the HTTP listener runs, from `sv_httpEnabled`. Only `false` and
    /// `0` turn it off.
    pub(crate) fn http_enabled(&self) -> bool {
        self.get(HTTP_ENABLED).is_none_or(|value| {
            let value = value.trim();
            !(value.eq_ignore_ascii_case("false") || value == "0")
        })
    }

    /// How long a resource's HTTP handler may take to complete its response,
    /// from `sixm_httpHandlerTimeout`. Clamped to `1` to `120` seconds; a value
    /// that is not a number means 25.
    pub(crate) fn http_handler_timeout(&self) -> Duration {
        let seconds = self
            .get(HTTP_HANDLER_TIMEOUT)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_HTTP_HANDLER_TIMEOUT);
        Duration::from_secs(
            seconds
                .clamp(MIN_HTTP_HANDLER_TIMEOUT, MAX_HTTP_HANDLER_TIMEOUT)
                .unsigned_abs(),
        )
    }

    /// Whether `/players.json` lists player identifiers, from
    /// `sv_exposePlayerIdentifiers`. Only the value `true` turns it on.
    pub(crate) fn expose_player_identifiers(&self) -> bool {
        self.get(EXPOSE_IDENTIFIERS)
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
    }

    /// Updates the credential outside the convar map exposed to scripts and clients.
    pub(crate) fn set_rcon_password(&mut self, password: &str) {
        self.rcon_password = (!password.is_empty()).then(|| digest(&SHA256, password.as_bytes()));
    }

    /// The configured digest, or `None` when RCON is disabled.
    pub(crate) fn rcon_password(&self) -> Option<&Digest> {
        self.rcon_password.as_ref()
    }

    /// The model name of each player's ped, from `sixm_playerModel`.
    pub(crate) fn player_model(&self) -> &str {
        self.get(PLAYER_MODEL).unwrap_or(DEFAULT_PLAYER_MODEL)
    }

    /// The radius of entity scoping in metres, from `sixm_cullRadius`. Clamped
    /// to `0` to `100000`; zero turns scoping off, and every player is then sent
    /// every entity. A value that is not a number means the default of 424.
    pub(crate) fn cull_radius(&self) -> f32 {
        self.values
            .get(CULL_RADIUS)
            .and_then(|entry| entry.value.trim().parse::<f32>().ok())
            .filter(|radius| radius.is_finite())
            .unwrap_or(DEFAULT_CULL_RADIUS)
            .clamp(0.0, MAX_CULL_RADIUS)
    }

    /// The most entities one client may create at once, from
    /// `sixm_maxClientEntities`. Clamped to `0..=4096`; a value that is not a
    /// number means the default of 64.
    pub(crate) fn max_client_entities(&self) -> usize {
        let value = self
            .get(MAX_CLIENT_ENTITIES)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_MAX_CLIENT_ENTITIES);
        usize::try_from(value.clamp(0, MAX_CLIENT_ENTITIES_LIMIT)).unwrap_or(0)
    }

    /// The global entity lockdown mode, from `sv_entityLockdown`: `inactive`,
    /// `relaxed` or `strict`. Any other value falls back to `inactive`.
    pub(crate) fn entity_lockdown(&self) -> &str {
        match self.get(ENTITY_LOCKDOWN).map(str::trim) {
            Some("relaxed") => "relaxed",
            Some("strict") => "strict",
            _ => DEFAULT_ENTITY_LOCKDOWN,
        }
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
    fn http_workers_default_clamp_and_fall_back() {
        let mut convars = Convars::new();
        assert_eq!(convars.http_workers(), 8);
        for (value, expected) in [("3", 3), ("0", 1), ("1000", 64), ("many", 8)] {
            convars.set(HTTP_WORKERS, value, ConvarKind::Plain);
            assert_eq!(convars.http_workers(), expected, "{value}");
        }
    }

    #[test]
    fn http_handler_timeout_default_clamp_and_fall_back() {
        let mut convars = Convars::new();
        assert_eq!(convars.http_handler_timeout(), Duration::from_secs(25));
        for (value, expected) in [("1", 1), ("0", 1), ("500", 120), ("slow", 25)] {
            convars.set(HTTP_HANDLER_TIMEOUT, value, ConvarKind::Plain);
            assert_eq!(
                convars.http_handler_timeout(),
                Duration::from_secs(expected),
                "{value}"
            );
        }
    }

    #[test]
    fn http_is_on_unless_switched_off() {
        let mut convars = Convars::new();
        assert!(convars.http_enabled());
        for (value, expected) in [("false", false), ("0", false), ("true", true), ("1", true)] {
            convars.set(HTTP_ENABLED, value, ConvarKind::Plain);
            assert_eq!(convars.http_enabled(), expected, "{value}");
        }
    }

    #[test]
    fn identifiers_are_hidden_unless_exposed() {
        let mut convars = Convars::new();
        assert!(!convars.expose_player_identifiers());
        convars.set(EXPOSE_IDENTIFIERS, "true", ConvarKind::Plain);
        assert!(convars.expose_player_identifiers());
        convars.set(EXPOSE_IDENTIFIERS, "1", ConvarKind::Plain);
        assert!(!convars.expose_player_identifiers());
    }

    #[test]
    fn built_in_convars_are_plain_but_the_client_entity_limit() {
        let convars = Convars::new();
        assert_eq!(convars.kind(SERVER_NAME), Some(ConvarKind::Plain));
        assert_eq!(
            convars.replicated(),
            vec![(MAX_CLIENT_ENTITIES.to_ascii_lowercase(), "64".to_owned())]
        );
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
    fn the_cull_radius_is_clamped_and_zero_turns_scoping_off() {
        let mut convars = Convars::new();
        assert_eq!(convars.cull_radius(), 424.0);
        convars.set("sixm_cullRadius", "50", ConvarKind::Plain);
        assert_eq!(convars.cull_radius(), 50.0);
        convars.set("SIXM_CULLRADIUS", "0", ConvarKind::Plain);
        assert_eq!(convars.cull_radius(), 0.0);
        convars.set("sixm_cullRadius", "-5", ConvarKind::Plain);
        assert_eq!(convars.cull_radius(), 0.0);
        convars.set("sixm_cullRadius", "1e9", ConvarKind::Plain);
        assert_eq!(convars.cull_radius(), 100_000.0);
        convars.set("sixm_cullRadius", "far", ConvarKind::Plain);
        assert_eq!(convars.cull_radius(), 424.0);
        convars.set("sixm_cullRadius", "NaN", ConvarKind::Plain);
        assert_eq!(convars.cull_radius(), 424.0);
    }

    #[test]
    fn only_replicated_convars_are_listed_with_their_latest_value() {
        let mut convars = Convars::new();
        convars.set("a", "1", ConvarKind::Plain);
        convars.set("b", "2", ConvarKind::ServerInfo);
        convars.set("d", "4", ConvarKind::Replicated);
        convars.set("c", "3", ConvarKind::Replicated);
        convars.set("c", "5", ConvarKind::Replicated);
        let default_replicated = (MAX_CLIENT_ENTITIES.to_ascii_lowercase(), "64".to_owned());
        assert_eq!(
            convars.replicated(),
            [
                ("c".to_owned(), "5".to_owned()),
                ("d".to_owned(), "4".to_owned()),
                default_replicated.clone(),
            ]
        );

        convars.set("c", "gone", ConvarKind::Plain);
        assert_eq!(
            convars.replicated(),
            [("d".to_owned(), "4".to_owned()), default_replicated]
        );
    }

    #[test]
    fn entity_lockdown_accepts_known_modes_and_falls_back_to_inactive() {
        let mut convars = Convars::new();
        assert_eq!(convars.entity_lockdown(), "inactive");
        for (value, expected) in [
            ("relaxed", "relaxed"),
            ("strict", "strict"),
            ("bogus", "inactive"),
            ("inactive", "inactive"),
        ] {
            convars.set(ENTITY_LOCKDOWN, value, ConvarKind::Plain);
            assert_eq!(convars.entity_lockdown(), expected, "{value}");
        }
    }
}
