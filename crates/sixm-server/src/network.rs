//! Networking: the listening socket, the player table and the events that cross
//! the network.
//!
//! The network thread reports through [`Input::Net`], and everything here runs
//! on the server's loop thread. Decisions about a connection are made during
//! the `playerConnecting` dispatch, so a connection is always accepted or
//! refused before the loop returns to the network.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use sixm_core::{ClientTarget, EventSource, PlayerInfo, ScriptError, Value};
use sixm_net::{ConnectionId, Fingerprint, NetError, NetEvent, NetServer, ServerConfig};
use tracing::{debug, info, warn};

use crate::commands::{self, CommandContext};
use crate::host_functions::HostFn;
use crate::manager::ResourceManager;
use crate::server::Input;

/// Refusal sent when a script cancels a connection without giving a reason.
const DEFAULT_REFUSAL: &str = "Rejected by the server";
/// Refusal sent when `sv_maxclients` players are already connected.
const FULL_REASON: &str = "Server is full";

/// A player, from the moment its connection is decided on until it leaves.
#[derive(Debug)]
pub(crate) struct Player {
    pub(crate) name: String,
    pub(crate) endpoint: SocketAddr,
    /// The connection the player came in on.
    connection: ConnectionId,
    /// `license:<fingerprint>` and `ip:<address>`, in that order.
    identifiers: Vec<String>,
    /// The identifiers as access control principals, `identifier.<identifier>`,
    /// in lower case.
    principals: Vec<String>,
    /// Whether `playerConnecting` let the player in. Until then the player is
    /// known to `player_info` and the identifier functions only.
    admitted: bool,
}

impl Player {
    fn pending(
        name: &str,
        endpoint: SocketAddr,
        connection: ConnectionId,
        fingerprint: Fingerprint,
    ) -> Self {
        let identifiers = vec![
            format!("license:{fingerprint}"),
            format!("ip:{}", endpoint.ip()),
        ];
        let principals = identifiers
            .iter()
            .map(|identifier| format!("identifier.{identifier}"))
            .collect();
        Self {
            name: name.to_owned(),
            endpoint,
            connection,
            identifiers,
            principals,
            admitted: false,
        }
    }
}

/// Players by server id. Ids count up from 1 and are never reused, so the
/// map's order is also the order in which players joined. A player is listed
/// and counted against the limit only once it is admitted.
#[derive(Debug)]
pub(crate) struct Players {
    /// Next id to hand out. Zero once every id has been used.
    next_id: u32,
    by_id: BTreeMap<u32, Player>,
}

impl Default for Players {
    fn default() -> Self {
        Self {
            next_id: 1,
            by_id: BTreeMap::new(),
        }
    }
}

impl Players {
    /// A fresh id for a new connection, or `None` once all `u32` ids are used.
    fn allocate(&mut self) -> Option<u32> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).unwrap_or(0);
        (id != 0).then_some(id)
    }

    /// How many players are admitted.
    fn admitted_count(&self) -> usize {
        self.by_id.values().filter(|player| player.admitted).count()
    }

    /// Whether `id` is an admitted player.
    pub(crate) fn contains(&self, id: u32) -> bool {
        self.get(id).is_some_and(|player| player.admitted)
    }

    /// The admitted players' ids, in join order.
    fn admitted_ids(&self) -> Vec<u32> {
        self.by_id
            .iter()
            .filter(|(_, player)| player.admitted)
            .map(|(id, _)| *id)
            .collect()
    }

    /// The player, admitted or not, that `id` names.
    fn get(&self, id: u32) -> Option<&Player> {
        self.by_id.get(&id)
    }

    /// The player that came in on `connection`, if there is one.
    fn of_connection(&self, connection: ConnectionId) -> Option<u32> {
        self.by_id
            .iter()
            .find(|(_, player)| player.connection == connection)
            .map(|(id, _)| *id)
    }

    fn insert(&mut self, id: u32, player: Player) {
        self.by_id.insert(id, player);
    }

    /// Admits the pending player `id`.
    fn admit(&mut self, id: u32) {
        if let Some(player) = self.by_id.get_mut(&id) {
            player.admitted = true;
        }
    }

    fn remove(&mut self, id: u32) -> Option<Player> {
        self.by_id.remove(&id)
    }
}

/// Everything the network needs: the socket, the players and kicks waiting
/// for the end of the current script call.
#[derive(Debug, Default)]
pub(crate) struct Network {
    /// The listening socket, while the network runs.
    pub(crate) server: RefCell<Option<NetServer>>,
    pub(crate) players: RefCell<Players>,
    /// Kicks requested by `DropPlayer`, applied by [`ResourceManager::apply_drops`].
    pub(crate) pending_drops: RefCell<Vec<(u32, String)>>,
}

impl ResourceManager {
    /// Binds the listening socket and forwards its events to `input`.
    ///
    /// Returns the bound address, with the port resolved. Fails when the
    /// endpoint is not an address, when the socket cannot be bound, or when the
    /// network is already running.
    pub(crate) fn start_network(&self, input: Sender<Input>) -> Result<SocketAddr, NetError> {
        if self.network.server.borrow().is_some() {
            return Err(NetError::Config(
                "the network is already running".to_owned(),
            ));
        }
        let (endpoint, server_name, identity) = {
            let convars = self.convars.borrow();
            (
                convars.endpoint().to_owned(),
                convars.server_name().to_owned(),
                convars.identity(),
            )
        };
        let bind: SocketAddr = endpoint.parse().map_err(|_| {
            NetError::Config(format!(
                "endpoint_add_udp `{endpoint}` is not an ip:port address"
            ))
        })?;
        let config = ServerConfig {
            bind,
            server_name,
            identity,
            ..ServerConfig::default()
        };
        let server = NetServer::bind(config, move |event| {
            // The loop is gone once the receiver is dropped; events are then moot.
            let _ = input.send(Input::Net(event));
        })?;
        let addr = server.local_addr();
        *self.network.server.borrow_mut() = Some(server);
        self.publish_advertised();
        Ok(addr)
    }

    /// The fingerprint of the certificate clients are shown, while the network runs.
    pub(crate) fn network_fingerprint(&self) -> Option<Fingerprint> {
        self.network
            .server
            .borrow()
            .as_ref()
            .map(NetServer::fingerprint)
    }

    /// Closes every connection and joins the network thread. Does nothing when
    /// the network is not running.
    pub(crate) fn stop_network(&self) {
        let server = self.network.server.borrow_mut().take();
        if let Some(server) = server {
            server.shutdown();
        }
    }

    /// Handles one event from the network thread.
    pub(crate) fn handle_net(&self, event: NetEvent) {
        match event {
            NetEvent::Connecting {
                connection,
                name,
                endpoint,
                fingerprint,
            } => self.connecting(connection, &name, endpoint, fingerprint),
            NetEvent::Event { player, name, args } => self.player_event(player, &name, &args),
            NetEvent::Command { player, line } => self.player_command(player, &line),
            NetEvent::Disconnected { player, reason } => self.player_dropped(player, &reason),
            NetEvent::Abandoned { connection } => self.abandoned(connection),
        }
    }

    /// Admits or refuses a new connection: assigns its id, runs
    /// `playerConnecting` with the player already known, then checks the
    /// player limit.
    fn connecting(
        &self,
        connection: ConnectionId,
        name: &str,
        endpoint: SocketAddr,
        fingerprint: Fingerprint,
    ) {
        let allocated = self.network.players.borrow_mut().allocate();
        let Some(id) = allocated else {
            self.refuse(connection, name, endpoint, FULL_REASON);
            return;
        };
        self.network
            .players
            .borrow_mut()
            .insert(id, Player::pending(name, endpoint, connection, fingerprint));

        if let Some(reason) = self.connecting_refusal(id, name) {
            self.network.players.borrow_mut().remove(id);
            self.refuse(connection, name, endpoint, &reason);
            return;
        }

        self.network.players.borrow_mut().admit(id);
        self.with_server(|server| server.accept(connection, id));
        // The resource list goes last: a client waits for its end before it
        // starts any script, so the convars, entities and state sent before it
        // are already applied when the scripts load.
        self.join_convars(id);
        self.join_entities(id);
        self.join_state(id);
        self.with_server(|server| {
            if let Err(err) = server.send_resources(id) {
                warn!("cannot send the resource list to player {id}: {err}");
            }
        });
        self.dispatch("playerJoining", EventSource::Player(id), &[]);
        info!("{name} ({id}) joined from {endpoint}");
    }

    /// Runs `playerConnecting` for the pending player `id` and checks the
    /// player limit. Returns the reason to refuse the player, if there is one.
    fn connecting_refusal(&self, id: u32, name: &str) -> Option<String> {
        let kick_reason = Rc::new(RefCell::new(None::<String>));
        let sink = Rc::clone(&kick_reason);
        let set_kick_reason: HostFn = Rc::new(move |args: &[Value]| {
            let reason = args
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| ScriptError::Runtime("setKickReason expects a string".to_owned()))?;
            *sink.borrow_mut() = Some(reason.to_owned());
            Ok(Vec::new())
        });
        let function = self.host_functions.borrow_mut().add(set_kick_reason);
        let canceled = self.dispatch(
            "playerConnecting",
            EventSource::Player(id),
            &[Value::from(name), Value::FuncRef(function.clone())],
        );
        // Function references are valid only for the dispatch, so the host
        // function goes now. A handler that kept it gets a stale-reference error.
        self.host_functions.borrow_mut().remove(&function);

        let reason = kick_reason.borrow_mut().take();
        if canceled || reason.is_some() {
            return Some(
                reason
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| DEFAULT_REFUSAL.to_owned()),
            );
        }

        let full =
            self.network.players.borrow().admitted_count() >= self.convars.borrow().max_clients();
        full.then(|| FULL_REASON.to_owned())
    }

    /// Handles a connection that closed before the server could admit it. If the
    /// connection was already admitted, the player leaves now: the network never
    /// reports such a player as disconnected.
    fn abandoned(&self, connection: ConnectionId) {
        let player = self.network.players.borrow().of_connection(connection);
        if let Some(player) = player {
            self.player_dropped(player, "connection closed");
        } else {
            debug!("connection {connection} closed before it was admitted");
        }
    }

    /// Refuses a connection that has not joined, and logs why.
    fn refuse(&self, connection: ConnectionId, name: &str, endpoint: SocketAddr, reason: &str) {
        self.with_server(|server| server.reject(connection, reason));
        info!("{name} from {endpoint} refused: {reason}");
    }

    /// Delivers a player's event to the handlers registered with `RegisterNetEvent`.
    fn player_event(&self, player: u32, name: &str, args: &[Value]) {
        if !self.network.players.borrow().contains(player) {
            debug!(player, "event `{name}` from an unknown player ignored");
            return;
        }
        self.dispatch(name, EventSource::Net(player), args);
    }

    /// Runs a console line a player sent, with the player's permissions.
    fn player_command(&self, player: u32, line: &str) {
        if !self.network.players.borrow().contains(player) {
            debug!(player, "command from an unknown player ignored");
            return;
        }
        commands::run_line(self, line, CommandContext::player(player));
    }

    /// Whether connected player `player` may use `object`. The player is the
    /// principal `player.<id>`.
    pub(crate) fn player_may(&self, player: u32, object: &str) -> bool {
        self.ace_allowed(&format!("player.{player}"), object)
    }

    /// Whether `principal` may use `object` under the access control entries.
    /// A principal `player.<id>` of a connected or connecting player also
    /// inherits from its identifiers.
    pub(crate) fn ace_allowed(&self, principal: &str, object: &str) -> bool {
        let players = self.network.players.borrow();
        let player = principal
            .strip_prefix("player.")
            .and_then(|text| text.parse::<u32>().ok())
            .and_then(|id| players.get(id));
        self.access.borrow().is_allowed(
            principal,
            object,
            player
                .into_iter()
                .flat_map(|found| found.principals.iter().map(String::as_str)),
        )
    }

    /// Runs `playerDropped` and removes the player. The player is still listed
    /// while the handlers run, so they can look up who left.
    fn player_dropped(&self, player: u32, reason: &str) {
        let name = self
            .network
            .players
            .borrow()
            .get(player)
            .map(|found| found.name.clone());
        let Some(name) = name else {
            debug!(player, "disconnect of an unknown player ignored");
            return;
        };
        self.dispatch(
            "playerDropped",
            EventSource::Player(player),
            &[Value::from(reason)],
        );
        self.network.players.borrow_mut().remove(player);
        self.leave_state(player);
        info!("{name} ({player}) left: {reason}");
    }

    /// Runs `f` with the socket if the network is running.
    pub(crate) fn with_server(&self, f: impl FnOnce(&NetServer)) {
        if let Some(server) = self.network.server.borrow().as_ref() {
            f(server);
        }
    }

    /// Sends an event to one player or to every player. Without a running
    /// network nobody can receive it, so this succeeds and logs at debug level.
    pub(crate) fn send_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
    ) -> Result<(), ScriptError> {
        let server = self.network.server.borrow();
        let Some(server) = server.as_ref() else {
            debug!("TriggerClientEvent `{event}` ignored: the network is not running");
            return Ok(());
        };
        let result = match target {
            ClientTarget::Player(player) => server.send_event(player, event, args),
            ClientTarget::All => server.broadcast_event(event, args),
        };
        result.map_err(|err| ScriptError::Runtime(err.to_string()))
    }

    /// Server ids of the connected players, in join order.
    pub(crate) fn player_ids(&self) -> Vec<u32> {
        self.network.players.borrow().admitted_ids()
    }

    /// Name and endpoint of a player. A player who is still connecting is
    /// known too.
    pub(crate) fn player_details(&self, player: u32) -> Option<PlayerInfo> {
        self.network
            .players
            .borrow()
            .get(player)
            .map(|found| PlayerInfo {
                name: found.name.clone(),
                endpoint: found.endpoint.to_string(),
            })
    }

    /// The identifiers of a player, admitted or not. Empty for an unknown id.
    pub(crate) fn identifiers_of(&self, player: u32) -> Vec<String> {
        self.network
            .players
            .borrow()
            .get(player)
            .map(|found| found.identifiers.clone())
            .unwrap_or_default()
    }

    /// Every connected player with its id, in join order.
    pub(crate) fn player_list(&self) -> Vec<(u32, PlayerInfo)> {
        self.player_ids()
            .into_iter()
            .filter_map(|id| self.player_details(id).map(|info| (id, info)))
            .collect()
    }

    /// Queues a kick. It is applied at the next drain, after the current script call.
    pub(crate) fn queue_drop(&self, player: u32, reason: &str) {
        self.network
            .pending_drops
            .borrow_mut()
            .push((player, reason.to_owned()));
    }

    /// Kicks every player queued by [`queue_drop`](Self::queue_drop). A kick
    /// for a player who has already left does nothing.
    pub(crate) fn apply_drops(&self) {
        let drops = std::mem::take(&mut *self.network.pending_drops.borrow_mut());
        for (player, reason) in drops {
            self.with_server(|server| server.kick(player, &reason));
        }
    }
}
