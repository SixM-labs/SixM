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

use crate::host_functions::HostFn;
use crate::manager::ResourceManager;
use crate::server::Input;

/// Refusal sent when a script cancels a connection without giving a reason.
const DEFAULT_REFUSAL: &str = "Rejected by the server";
/// Refusal sent when `sv_maxclients` players are already connected.
const FULL_REASON: &str = "Server is full";

/// A connected player.
#[derive(Debug)]
pub(crate) struct Player {
    pub(crate) name: String,
    pub(crate) endpoint: SocketAddr,
}

/// Connected players by server id. Ids count up from 1 and are never reused, so
/// the map's order is also the order in which players joined.
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

    fn count(&self) -> usize {
        self.by_id.len()
    }

    pub(crate) fn contains(&self, id: u32) -> bool {
        self.by_id.contains_key(&id)
    }

    fn get(&self, id: u32) -> Option<&Player> {
        self.by_id.get(&id)
    }

    fn insert(&mut self, id: u32, player: Player) {
        self.by_id.insert(id, player);
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
            } => self.connecting(connection, &name, endpoint),
            NetEvent::Event { player, name, args } => self.player_event(player, &name, &args),
            NetEvent::Disconnected { player, reason } => self.player_dropped(player, &reason),
            // Connections are decided synchronously in `connecting`, so there is
            // no pending state to forget here.
            NetEvent::Abandoned { connection } => {
                debug!("connection {connection} closed before it was admitted");
            }
        }
    }

    /// Admits or refuses a new connection: assigns its id, runs
    /// `playerConnecting`, then checks the player limit.
    fn connecting(&self, connection: ConnectionId, name: &str, endpoint: SocketAddr) {
        let allocated = self.network.players.borrow_mut().allocate();
        let Some(id) = allocated else {
            self.refuse(connection, name, endpoint, FULL_REASON);
            return;
        };

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
            let text = reason
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| DEFAULT_REFUSAL.to_owned());
            self.refuse(connection, name, endpoint, &text);
            return;
        }

        let full = self.network.players.borrow().count() >= self.convars.borrow().max_clients();
        if full {
            self.refuse(connection, name, endpoint, FULL_REASON);
            return;
        }

        self.with_server(|server| server.accept(connection, id));
        self.network.players.borrow_mut().insert(
            id,
            Player {
                name: name.to_owned(),
                endpoint,
            },
        );
        self.with_server(|server| {
            if let Err(err) = server.send_resources(id) {
                warn!("cannot send the resource list to player {id}: {err}");
            }
        });
        self.join_state(id);
        self.dispatch("playerJoining", EventSource::Player(id), &[]);
        info!("{name} ({id}) joined from {endpoint}");
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
        self.network
            .players
            .borrow()
            .by_id
            .keys()
            .copied()
            .collect()
    }

    /// Name and endpoint of a connected player.
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
