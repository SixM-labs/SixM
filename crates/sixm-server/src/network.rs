//! Networking: the listening socket, the player table and the events that cross
//! the network.
//!
//! The network thread reports through [`Input::Net`], and everything here runs
//! on the server's loop thread. A connection is accepted or refused during the
//! `playerConnecting` dispatch, or later once the handlers that deferred it
//! call `done`. Script calls never run inside these functions: deferral calls
//! are queued and applied by [`ResourceManager::apply_deferral_calls`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use sixm_core::{ClientTarget, EventSource, FuncRef, PlayerInfo, ScriptError, Value};
use sixm_net::{
    ConnectionId, Fingerprint, NetError, NetEvent, NetServer, ServerConfig, Traffic, WebServer,
};
use tracing::{debug, error, info, warn};

use crate::commands::{self, CommandContext, CommandSource};
use crate::host_functions::HostFn;
use crate::manager::ResourceManager;
use crate::server::Input;

/// Refusal sent when a script cancels a connection without giving a reason.
const DEFAULT_REFUSAL: &str = "Rejected by the server";
/// Refusal sent when `sv_maxclients` players are already connected.
const FULL_REASON: &str = "Server is full";
/// Refusal sent when a deferred connection is not finished in time.
const DEFERRAL_TIMEOUT_REASON: &str = "connection timed out";
/// Extra port choices when the system-assigned UDP port is unavailable to TCP.
const HTTP_PORT_RETRIES: usize = 4;
/// Minimum interval between anti-flood drop warnings per player.
const FLOOD_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// Per-player token bucket for rate-limiting incoming `NetEvent::Event` messages.
#[derive(Debug)]
struct EventBucket {
    tokens: f64,
    last_refill: Instant,
    last_warn: Option<Instant>,
    dropped_since_warn: u64,
}

impl EventBucket {
    fn new(now: Instant, rate_per_sec: u32) -> Self {
        Self {
            tokens: f64::from(rate_per_sec) * 2.0,
            last_refill: now,
            last_warn: None,
            dropped_since_warn: 0,
        }
    }

    fn allow(&mut self, now: Instant, rate_per_sec: u32) -> (bool, Option<u64>) {
        let rate = f64::from(rate_per_sec);
        let burst = rate * 2.0;
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * rate).min(burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return (true, None);
        }
        self.dropped_since_warn += 1;
        let should_warn = self
            .last_warn
            .is_none_or(|prev| now.saturating_duration_since(prev) >= FLOOD_WARN_INTERVAL);
        if should_warn {
            self.last_warn = Some(now);
            let dropped = self.dropped_since_warn;
            self.dropped_since_warn = 0;
            (false, Some(dropped))
        } else {
            (false, None)
        }
    }
}

/// A connection that `playerConnecting` deferred and no `done` has ended yet.
#[derive(Debug)]
struct Deferral {
    /// `defer` calls not yet matched by a `done` without a reason.
    open: u32,
    /// When the connection was deferred, for the timeout.
    since: Instant,
}

/// A call to one of the deferral functions. The call is queued while the
/// script runs and applied once the script call returns.
#[derive(Debug)]
pub(crate) enum DeferralCall {
    /// `defer()`: one more handler holds the connection.
    Defer,
    /// `update(message)`: progress text for the client.
    Update(String),
    /// `done(reason)`: the handler is finished. A reason refuses the player.
    Done(Option<String>),
}

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
    /// Set while handlers hold the connection with `defer`.
    deferral: Option<Deferral>,
    /// The `update` and `done` functions of the `playerConnecting` table. They
    /// are removed when the player is admitted or refused.
    functions: Vec<FuncRef>,
    /// The player's own ped, created when it is admitted and deleted when it
    /// leaves.
    ped: Option<u32>,
    /// When the last message arrived from this player.
    last_msg: Instant,
    /// Deterministic client tokens derived from the player's TLS certificate fingerprint.
    tokens: Vec<String>,
    /// Token-bucket rate limiter for `NetEvent::Event`.
    event_bucket: Option<EventBucket>,
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
        let tokens = vec![format!("0:{fingerprint}")];
        Self {
            name: name.to_owned(),
            endpoint,
            connection,
            identifiers,
            principals,
            admitted: false,
            deferral: None,
            functions: Vec::new(),
            ped: None,
            last_msg: Instant::now(),
            tokens,
            event_bucket: None,
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
    #[cfg(test)]
    test_peds: BTreeMap<u32, Option<u32>>,
}

impl Default for Players {
    fn default() -> Self {
        Self {
            next_id: 1,
            by_id: BTreeMap::new(),
            #[cfg(test)]
            test_peds: BTreeMap::new(),
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
        #[cfg(test)]
        if self.test_peds.contains_key(&id) {
            return true;
        }
        self.get(id).is_some_and(|player| player.admitted)
    }

    #[cfg(test)]
    pub(crate) fn insert_test_player(&mut self, id: u32, ped: Option<u32>) {
        self.test_peds.insert(id, ped);
    }

    #[cfg(test)]
    pub(crate) fn remove_test_player(&mut self, id: u32) {
        self.test_peds.remove(&id);
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

    /// The player, admitted or not, that `id` names, for changing it.
    fn get_mut(&mut self, id: u32) -> Option<&mut Player> {
        self.by_id.get_mut(&id)
    }

    /// The deferred players whose deferral started at least `timeout` before `now`.
    /// Allocates only when one has expired.
    fn expired(&self, now: Instant, timeout: Duration) -> Vec<u32> {
        self.by_id
            .iter()
            .filter_map(|(id, player)| {
                player
                    .deferral
                    .as_ref()
                    .filter(|deferral| now.saturating_duration_since(deferral.since) >= timeout)
                    .map(|_| *id)
            })
            .collect()
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
    /// The HTTP listener on the same address, while the network runs.
    pub(crate) web: RefCell<Option<WebServer>>,
    pub(crate) players: RefCell<Players>,
    /// Kicks requested by `DropPlayer`, applied by [`ResourceManager::apply_drops`].
    pub(crate) pending_drops: RefCell<Vec<(u32, String)>>,
    /// Deferral calls by player, in the order the scripts made them. Shared
    /// with the deferral functions, which push onto it.
    pub(crate) deferral_calls: Rc<RefCell<Vec<(u32, DeferralCall)>>>,
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
        let (endpoint, server_name, identity, http_enabled) = {
            let convars = self.convars.borrow();
            (
                convars.endpoint().to_owned(),
                convars.server_name().to_owned(),
                convars.identity(),
                convars.http_enabled(),
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
        let net_input = input.clone();
        let bind_game = || {
            let net_input = net_input.clone();
            NetServer::bind(config.clone(), move |event| {
                let _ = net_input.send(Input::Net(event));
            })
        };
        let mut server = bind_game()?;

        // A port selected by TCP may be unavailable to UDP. Bind the game
        // socket first and let HTTP use its address when TCP can bind it.
        let web = if http_enabled {
            let on_web = move |exchange| {
                let _ = input.send(Input::Web(exchange));
            };
            let mut retries = if bind.port() == 0 {
                HTTP_PORT_RETRIES
            } else {
                0
            };
            loop {
                let error = match WebServer::bind(server.local_addr(), on_web.clone()) {
                    Ok(web) => break Some(web),
                    Err(error) => error,
                };
                if retries == 0
                    || !matches!(&error, NetError::Bind { source, .. }
                    if matches!(source.kind(), ErrorKind::AddrInUse | ErrorKind::PermissionDenied))
                {
                    error!("HTTP is off: {error}");
                    break None;
                }

                // Hold the current UDP port until a replacement has been chosen.
                retries -= 1;
                match bind_game() {
                    Ok(replacement) => server = replacement,
                    Err(retry_error) => {
                        error!(
                            "HTTP is off: {error}. Could not choose another game port: {retry_error}"
                        );
                        break None;
                    }
                }
            }
        } else {
            info!("HTTP is off: sv_httpEnabled is false");
            None
        };
        let addr = server.local_addr();
        *self.network.web.borrow_mut() = web;
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
        let web = self.network.web.borrow_mut().take();
        drop(web);
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
            NetEvent::Event { player, name, args } => {
                self.touch_player(player);
                self.player_event(player, &name, &args);
            }
            NetEvent::Command { player, line } => {
                self.touch_player(player);
                self.player_command(player, &line);
            }
            NetEvent::EntityTransforms { player, updates } => {
                self.touch_player(player);
                self.player_transforms(player, &updates);
            }
            NetEvent::CreateEntity {
                player,
                id,
                kind,
                model,
                position,
                rotation,
            } => {
                self.touch_player(player);
                self.client_create_entity(player, id, kind, model, position, rotation);
            }
            NetEvent::RequestEntityControl { player, id } => {
                self.touch_player(player);
                self.client_request_control(player, id);
            }
            NetEvent::Disconnected { player, reason } => self.player_dropped(player, &reason),
            NetEvent::Abandoned { connection } => self.abandoned(connection),
        }
    }

    /// Admits, defers or refuses a new connection: assigns its id, runs
    /// `playerConnecting` with the player already known, applies the deferral
    /// calls it made and settles the player.
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

        if let Some(reason) = self.run_connecting(id, name) {
            self.refuse_pending(id, &reason);
            return;
        }
        self.apply_deferral_calls();
        self.settle(id);
    }

    /// Runs `playerConnecting` for the pending player `id`. Returns the reason
    /// to refuse the player when a handler canceled the connection or set a
    /// kick reason. Otherwise the player is left for [`settle`](Self::settle).
    fn run_connecting(&self, id: u32, name: &str) -> Option<String> {
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
        let (defer, update, done) = self.deferral_handlers(id);
        let (kick_ref, defer_ref, update_ref, done_ref) = {
            let mut functions = self.host_functions.borrow_mut();
            (
                functions.add(set_kick_reason),
                functions.add(defer),
                functions.add(update),
                functions.add(done),
            )
        };
        let table = Value::Map(vec![
            (Value::from("defer"), Value::FuncRef(defer_ref.clone())),
            (Value::from("update"), Value::FuncRef(update_ref.clone())),
            (Value::from("done"), Value::FuncRef(done_ref.clone())),
        ]);
        let canceled = self.dispatch(
            "playerConnecting",
            EventSource::Player(id),
            &[Value::from(name), Value::FuncRef(kick_ref.clone()), table],
        );
        // Function references are valid only for the dispatch, so
        // `setKickReason` and `defer` go now. A handler that kept them gets a
        // stale-reference error. `update` and `done` stay for as long as the
        // connection is held.
        {
            let mut functions = self.host_functions.borrow_mut();
            functions.remove(&kick_ref);
            functions.remove(&defer_ref);
        }
        let functions = vec![update_ref, done_ref];
        let leftover = match self.network.players.borrow_mut().get_mut(id) {
            Some(player) => {
                player.functions = functions;
                None
            }
            None => Some(functions),
        };
        if let Some(functions) = leftover {
            self.release_functions(&functions);
        }

        let reason = kick_reason.borrow_mut().take();
        if canceled || reason.is_some() {
            return Some(
                reason
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| DEFAULT_REFUSAL.to_owned()),
            );
        }
        None
    }

    /// The `defer`, `update` and `done` functions of the `playerConnecting`
    /// table for player `id`. Each one queues its call, to be applied by
    /// [`apply_deferral_calls`](Self::apply_deferral_calls) after the script
    /// call returns. A `done` without a reason, or with an empty one, carries no
    /// reason at all.
    fn deferral_handlers(&self, id: u32) -> (HostFn, HostFn, HostFn) {
        let calls = Rc::clone(&self.network.deferral_calls);
        let defer_calls = Rc::clone(&calls);
        let defer: HostFn = Rc::new(move |_: &[Value]| {
            defer_calls.borrow_mut().push((id, DeferralCall::Defer));
            Ok(Vec::new())
        });
        let update_calls = Rc::clone(&calls);
        let update: HostFn = Rc::new(move |args: &[Value]| {
            let message = args.first().and_then(Value::as_str).ok_or_else(|| {
                ScriptError::Runtime("deferrals.update expects a string".to_owned())
            })?;
            update_calls
                .borrow_mut()
                .push((id, DeferralCall::Update(message.to_owned())));
            Ok(Vec::new())
        });
        let done: HostFn = Rc::new(move |args: &[Value]| {
            let reason = match args.first() {
                None | Some(Value::Nil) => None,
                Some(value) => Some(value.as_str().ok_or_else(|| {
                    ScriptError::Runtime("deferrals.done expects a string".to_owned())
                })?),
            };
            let reason = reason.filter(|text| !text.is_empty()).map(str::to_owned);
            calls.borrow_mut().push((id, DeferralCall::Done(reason)));
            Ok(Vec::new())
        });
        (defer, update, done)
    }

    /// Applies the deferral calls queued since the last time, in the order they
    /// were made, then settles every player they touched. Runs after each script
    /// call that can queue them, so no script call is ever made from here.
    pub(crate) fn apply_deferral_calls(&self) {
        let calls = std::mem::take(&mut *self.network.deferral_calls.borrow_mut());
        let mut touched: Vec<u32> = Vec::new();
        for (player, call) in calls {
            self.apply_deferral_call(player, call);
            if !touched.contains(&player) {
                touched.push(player);
            }
        }
        for player in touched {
            self.settle(player);
        }
    }

    /// Applies one deferral call to player `player`. A call for a player that
    /// is gone, or that the call does not apply to, does nothing.
    fn apply_deferral_call(&self, player: u32, call: DeferralCall) {
        match call {
            DeferralCall::Defer => {
                let mut players = self.network.players.borrow_mut();
                if let Some(found) = players.get_mut(player).filter(|found| !found.admitted) {
                    found
                        .deferral
                        .get_or_insert_with(|| Deferral {
                            open: 0,
                            since: Instant::now(),
                        })
                        .open += 1;
                }
            }
            DeferralCall::Update(message) => {
                let connection = self
                    .network
                    .players
                    .borrow()
                    .get(player)
                    .filter(|found| !found.admitted && found.deferral.is_some())
                    .map(|found| found.connection);
                if let Some(connection) = connection {
                    self.with_server(|server| {
                        if let Err(err) = server.update_deferral(connection, &message) {
                            warn!("cannot send a deferral update to player {player}: {err}");
                        }
                    });
                }
            }
            DeferralCall::Done(None) => {
                let mut players = self.network.players.borrow_mut();
                if let Some(found) = players.get_mut(player).filter(|found| !found.admitted) {
                    let ended = found.deferral.as_mut().is_some_and(|deferral| {
                        deferral.open = deferral.open.saturating_sub(1);
                        deferral.open == 0
                    });
                    if ended {
                        found.deferral = None;
                    }
                }
            }
            DeferralCall::Done(Some(reason)) => self.refuse_pending(player, &reason),
        }
    }

    /// Admits or refuses the player `id` when nothing holds its connection any
    /// more: it is admitted, or refused as full when the player limit is
    /// reached. A player that is admitted, gone or still deferred is left alone.
    fn settle(&self, id: u32) {
        let undecided = self
            .network
            .players
            .borrow()
            .get(id)
            .is_some_and(|player| !player.admitted && player.deferral.is_none());
        if !undecided {
            return;
        }
        let full =
            self.network.players.borrow().admitted_count() >= self.convars.borrow().max_clients();
        if full {
            self.refuse_pending(id, FULL_REASON);
        } else {
            self.admit(id);
        }
    }

    /// Admits the pending player `id`: creates its ped, accepts its connection,
    /// sends it the resources, entities and state, then runs `playerJoining`.
    fn admit(&self, id: u32) {
        let admitted = {
            let mut players = self.network.players.borrow_mut();
            players.get_mut(id).map(|player| {
                player.admitted = true;
                player.deferral = None;
                (
                    player.connection,
                    player.name.clone(),
                    player.endpoint,
                    std::mem::take(&mut player.functions),
                )
            })
        };
        let Some((connection, name, endpoint, functions)) = admitted else {
            return;
        };
        self.release_functions(&functions);
        // The ped is created before the connection is accepted, so the
        // creation is not sent to the player and only the join snapshot brings it.
        let ped = self.spawn_player_ped(id);
        if let Some(player) = self.network.players.borrow_mut().get_mut(id) {
            player.ped = ped;
        }
        self.with_server(|server| server.accept(connection, id));
        self.top_up_entity_ids(id);
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

    /// Refuses the pending player `id` with `reason`.
    fn refuse_pending(&self, id: u32, reason: &str) {
        if let Some(player) = self.end_pending(id) {
            self.refuse(player.connection, &player.name, player.endpoint, reason);
        }
    }

    /// Forgets the pending player `id`, releasing its deferral functions, and
    /// returns it. An admitted player is left alone.
    fn end_pending(&self, id: u32) -> Option<Player> {
        let player = {
            let mut players = self.network.players.borrow_mut();
            if players.get(id).is_none_or(|player| player.admitted) {
                return None;
            }
            players.remove(id)
        };
        if let Some(player) = &player {
            self.release_functions(&player.functions);
        }
        player
    }

    /// Removes the host functions in `functions`. A removed reference is a
    /// stale reference from then on.
    fn release_functions(&self, functions: &[FuncRef]) {
        let mut host_functions = self.host_functions.borrow_mut();
        for function in functions {
            host_functions.remove(function);
        }
    }

    /// Refuses every deferred player whose deferral has run past
    /// `sixm_deferralTimeout`. Runs every frame; it allocates only when a player
    /// has expired.
    pub(crate) fn expire_deferrals(&self, now: Instant) {
        let timeout = self.convars.borrow().deferral_timeout();
        let expired = self.network.players.borrow().expired(now, timeout);
        for player in expired {
            self.refuse_pending(player, DEFERRAL_TIMEOUT_REASON);
        }
    }

    /// Handles a connection that closed before the server decided it. An
    /// admitted player leaves now. A deferred player is forgotten at once, and
    /// neither `playerJoining` nor `playerDropped` runs for it.
    fn abandoned(&self, connection: ConnectionId) {
        let found = {
            let players = self.network.players.borrow();
            players
                .of_connection(connection)
                .map(|player| (player, players.contains(player)))
        };
        match found {
            Some((player, true)) => self.player_dropped(player, "connection closed"),
            Some((player, false)) => {
                if let Some(gone) = self.end_pending(player) {
                    info!(
                        "{} ({player}) closed the connection before it was admitted",
                        gone.name
                    );
                }
            }
            None => debug!("connection {connection} closed before it was admitted"),
        }
    }

    /// Refuses a connection that has not joined, and logs why.
    fn refuse(&self, connection: ConnectionId, name: &str, endpoint: SocketAddr, reason: &str) {
        self.with_server(|server| server.reject(connection, reason));
        info!("{name} from {endpoint} refused: {reason}");
    }

    /// Delivers a player's event to the handlers registered with `RegisterNetEvent`.
    fn player_event(&self, player: u32, name: &str, args: &[Value]) {
        let rate = self.convars.borrow().player_event_rate_limit();
        let (allowed, warn_dropped) = {
            let mut players = self.network.players.borrow_mut();
            let Some(found) = players.get_mut(player).filter(|found| found.admitted) else {
                debug!(player, "event `{name}` from an unknown player ignored");
                return;
            };
            if rate == 0 {
                found.event_bucket = None;
                (true, None)
            } else {
                let now = Instant::now();
                let bucket = found
                    .event_bucket
                    .get_or_insert_with(|| EventBucket::new(now, rate));
                bucket.allow(now, rate)
            }
        };
        if let Some(dropped) = warn_dropped {
            warn!(
                player,
                dropped, "dropping flooded server events (limit {rate}/s)"
            );
        }
        if !allowed {
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
        commands::run_line(
            self,
            line,
            &CommandContext::new(CommandSource::Player(player)),
        );
    }

    /// Whether connected player `player` may use `object`. The player is the
    /// principal `player.<id>`.
    pub(crate) fn player_may(&self, player: u32, object: &str) -> bool {
        self.ace_allowed(&format!("player.{player}"), object)
    }

    /// Whether the resource `resource` may use `object`. The resource is the
    /// principal `resource.<name>`, which inherits from `builtin.everyone`
    /// like every principal.
    pub(crate) fn resource_may(&self, resource: &str, object: &str) -> bool {
        self.ace_allowed(&format!("resource.{resource}"), object)
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

    /// Runs `playerDropped`, deletes the player's ped and removes the player.
    /// The player is still listed while the handlers run, so they can look up
    /// who left.
    fn player_dropped(&self, player: u32, reason: &str) {
        let found = self
            .network
            .players
            .borrow()
            .get(player)
            .map(|found| (found.name.clone(), found.ped));
        let Some((name, ped)) = found else {
            debug!(player, "disconnect of an unknown player ignored");
            return;
        };
        self.dispatch(
            "playerDropped",
            EventSource::Player(player),
            &[Value::from(reason)],
        );
        if let Some(ped) = ped {
            self.remove_entity(ped);
        }
        self.release_entities_of(player);
        self.leave_scope(player);
        self.network.players.borrow_mut().remove(player);
        self.leave_state(player);
        info!("{name} ({player}) left: {reason}");
    }

    /// The ped of the admitted player `player`, if it has one.
    pub(crate) fn ped_of_player(&self, player: u32) -> Option<u32> {
        let players = self.network.players.borrow();
        #[cfg(test)]
        if let Some(ped) = players.test_peds.get(&player) {
            return *ped;
        }
        players
            .get(player)
            .filter(|found| found.admitted)
            .and_then(|found| found.ped)
    }

    /// Traffic totals of the running network since it started, or zeros while
    /// it is not running.
    pub(crate) fn network_traffic(&self) -> Traffic {
        self.network
            .server
            .borrow()
            .as_ref()
            .map_or_else(Traffic::default, NetServer::traffic)
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

    /// Sends a paced event to one player or every player.
    pub(crate) fn send_latent_client_event(
        &self,
        event: &str,
        target: ClientTarget,
        args: &[Value],
        bps: u64,
    ) -> Result<(), ScriptError> {
        let server = self.network.server.borrow();
        let Some(server) = server.as_ref() else {
            debug!("TriggerLatentClientEvent `{event}` ignored: the network is not running");
            return Ok(());
        };
        let player = match target {
            ClientTarget::Player(id) => Some(id),
            ClientTarget::All => None,
        };
        server
            .send_latent_event(player, event, args, bps)
            .map_err(|err| ScriptError::Runtime(err.to_string()))
    }

    pub(crate) fn player_round_trip(&self, player: u32) -> Option<Duration> {
        let id = self.network.players.borrow().get(player)?.connection;
        self.network.server.borrow().as_ref()?.connection_ping(id)
    }

    fn touch_player(&self, player: u32) {
        if let Some(found) = self.network.players.borrow_mut().get_mut(player) {
            found.last_msg = Instant::now();
        }
    }

    pub(crate) fn player_last_message(&self, player: u32) -> Option<Duration> {
        self.network
            .players
            .borrow()
            .get(player)
            .map(|found| Instant::now().saturating_duration_since(found.last_msg))
    }

    pub(crate) fn player_tokens_of(&self, player: u32) -> Vec<String> {
        self.network
            .players
            .borrow()
            .get(player)
            .map(|found| found.tokens.clone())
            .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bucket_allows_burst_refills_and_rate_limits_warnings() {
        let t0 = Instant::now();
        let mut bucket = EventBucket::new(t0, 5); // burst = 10

        for _ in 0..10 {
            assert_eq!(bucket.allow(t0, 5), (true, None));
        }
        // 11th event at t0 is dropped with a warning count of 1.
        assert_eq!(bucket.allow(t0, 5), (false, Some(1)));
        // Subsequent immediate drops suppress warnings until FLOOD_WARN_INTERVAL.
        assert_eq!(bucket.allow(t0, 5), (false, None));
        assert_eq!(bucket.allow(t0, 5), (false, None));

        // After 1 second at 5/sec, 5 tokens refill.
        let t1 = t0 + Duration::from_secs(1);
        for _ in 0..5 {
            assert_eq!(bucket.allow(t1, 5), (true, None));
        }
        assert_eq!(bucket.allow(t1, 5), (false, None));

        // After FLOOD_WARN_INTERVAL without refill? Wait: 5s refills 10 tokens (capped at burst 10).
        let t2 = t0 + FLOOD_WARN_INTERVAL;
        for _ in 0..10 {
            assert_eq!(bucket.allow(t2, 5), (true, None));
        }
        // Next drop after FLOOD_WARN_INTERVAL warns with accumulated dropped count (4: 2 at t0 + 1 at t1 + 1 at t2).
        assert_eq!(bucket.allow(t2, 5), (false, Some(4)));
    }
}
