//! The server handle and the network thread that owns every connection.
//!
//! The server runs on a dedicated thread with its own tokio runtime. The
//! supervisor task on that thread owns all connection state. The owning thread
//! talks to it through an unbounded command channel, so every public method is
//! non-blocking, and reports back through the event callback.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use quinn::{Endpoint, Incoming, VarInt};
use sixm_core::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id, JoinError, JoinSet};
use tracing::{debug, error, warn};

use crate::connection::{self, Decision, Shared, Ticket};
use crate::error::NetError;
use crate::identity::{Credentials, Fingerprint, Identity};
use crate::protocol::{
    MAX_FRAME, OUTBOUND_QUEUE_LEN, SEND_QUEUE_OVERFLOW, SHUTTING_DOWN, ServerMessage,
};
use crate::transport;
use crate::wire::{encode_event, encode_state, encode_state_removed, encode_state_snapshot};

/// Default UDP port, the one FiveM clients expect.
const DEFAULT_PORT: u16 = 30120;
/// Time given to connections to close cleanly during shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Settings of a [`NetServer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Address to listen on. Port `0` picks a free port.
    pub bind: SocketAddr,
    /// Name sent to players in the welcome message.
    pub server_name: String,
    /// Time a new connection has to send its `Hello`.
    pub handshake_timeout: Duration,
    /// Largest frame accepted or sent, in bytes. Must not exceed 1 MiB, the
    /// largest frame clients accept.
    pub max_frame: usize,
    /// Where the certificate presented to clients comes from.
    pub identity: Identity,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
            server_name: "SixM Server".to_owned(),
            handshake_timeout: Duration::from_secs(10),
            max_frame: MAX_FRAME,
            identity: Identity::Ephemeral,
        }
    }
}

/// Identifies a connection that has not yet been accepted as a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Something that happened on the network, delivered to the event callback.
#[derive(Debug, Clone, PartialEq)]
pub enum NetEvent {
    /// A connection sent a valid `Hello`. The server must call
    /// [`NetServer::accept`] or [`NetServer::reject`] for it.
    Connecting {
        connection: ConnectionId,
        name: String,
        endpoint: SocketAddr,
    },
    /// A player sent an event.
    Event {
        player: u32,
        name: String,
        args: Vec<Value>,
    },
    /// An accepted player disconnected, was kicked, or broke the protocol.
    Disconnected { player: u32, reason: String },
    /// A connection left before it was accepted or rejected.
    Abandoned { connection: ConnectionId },
}

/// Commands from the owning thread to the network thread.
enum Command {
    Accept {
        connection: ConnectionId,
        player: u32,
    },
    Reject {
        connection: ConnectionId,
        reason: String,
    },
    Send {
        player: u32,
        frame: Arc<[u8]>,
    },
    Broadcast {
        frame: Arc<[u8]>,
    },
    Kick {
        player: u32,
        reason: String,
    },
}

/// Handle to the server's network thread.
///
/// Dropping the handle, or calling [`NetServer::shutdown`], closes every
/// connection and joins the network thread.
pub struct NetServer {
    commands: Option<mpsc::UnboundedSender<Command>>,
    local_addr: SocketAddr,
    fingerprint: Fingerprint,
    max_frame: usize,
    thread: Option<JoinHandle<()>>,
}

impl NetServer {
    /// Starts listening on `config.bind` and spawns the network thread.
    ///
    /// The certificate comes from `config.identity`; its fingerprint is
    /// available from [`fingerprint`](Self::fingerprint). `on_event` runs on
    /// the network thread, so it must not block.
    pub fn bind(
        config: ServerConfig,
        on_event: impl Fn(NetEvent) + Send + Sync + 'static,
    ) -> Result<Self, NetError> {
        if config.max_frame == 0 || config.max_frame > MAX_FRAME {
            return Err(NetError::Config(format!(
                "max_frame must be between 1 and {MAX_FRAME} bytes, got {}",
                config.max_frame
            )));
        }
        let max_frame = config.max_frame;
        let bind = config.bind;
        let credentials = Credentials::load(&config.identity)?;
        let fingerprint = credentials.fingerprint();
        let shared = Arc::new(Shared {
            on_event: Arc::new(on_event),
            server_name: config.server_name,
            handshake_timeout: config.handshake_timeout,
            max_frame,
        });
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = sync_channel(1);
        let thread = thread::Builder::new()
            .name("sixm-net".to_owned())
            .spawn(move || run_thread(bind, credentials, shared, command_rx, ready_tx))
            .map_err(NetError::Io)?;
        let local_addr = ready_rx.recv().map_err(|_| NetError::Closed)??;
        Ok(Self {
            commands: Some(commands),
            local_addr,
            fingerprint,
            max_frame,
            thread: Some(thread),
        })
    }

    /// Address the server is listening on, with the port resolved.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Fingerprint of the certificate clients are shown. Clients that pass it
    /// to [`NetClient::connect_pinned`](crate::NetClient::connect_pinned)
    /// accept only this server.
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// Admits a pending connection as `player` and sends it the welcome.
    /// Does nothing if the connection is no longer pending.
    pub fn accept(&self, connection: ConnectionId, player: u32) {
        self.post_lossy(Command::Accept { connection, player });
    }

    /// Refuses a pending connection with `reason` and closes it gracefully.
    /// Does nothing if the connection is no longer pending.
    pub fn reject(&self, connection: ConnectionId, reason: &str) {
        self.post_lossy(Command::Reject {
            connection,
            reason: reason.to_owned(),
        });
    }

    /// Sends an event to one player. Sending to a player that is not connected
    /// is a no-op.
    ///
    /// The arguments are encoded on the calling thread, so the cost of a large
    /// value is paid there and not on the network thread.
    pub fn send_event(&self, player: u32, name: &str, args: &[Value]) -> Result<(), NetError> {
        let frame = encode_event(ServerMessage::EVENT_TAG, name, args, self.max_frame)?;
        self.post(Command::Send { player, frame })
    }

    /// Sends an event to every accepted player. The message is encoded once.
    pub fn broadcast_event(&self, name: &str, args: &[Value]) -> Result<(), NetError> {
        let frame = encode_event(ServerMessage::EVENT_TAG, name, args, self.max_frame)?;
        self.post(Command::Broadcast { frame })
    }

    /// Sends one state bag key to one player. A nil `value` tells the player
    /// that the key was deleted. The message is encoded on the calling thread.
    pub fn send_state(
        &self,
        player: u32,
        bag: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), NetError> {
        let frame = encode_state(bag, key, value, self.max_frame)?;
        self.post(Command::Send { player, frame })
    }

    /// Sends several state bag keys to one player, in order, as the fewest
    /// frames that fit. The player sees one state event per entry. Entries
    /// too large for any frame are skipped with a warning. Names and values
    /// are validated before anything is sent.
    pub fn send_state_snapshot(
        &self,
        player: u32,
        entries: &[(&str, &str, &Value)],
    ) -> Result<(), NetError> {
        for frame in encode_state_snapshot(entries, self.max_frame)? {
            self.post(Command::Send { player, frame })?;
        }
        Ok(())
    }

    /// Sends one state bag key to every accepted player. The message is encoded once.
    pub fn broadcast_state(&self, bag: &str, key: &str, value: &Value) -> Result<(), NetError> {
        let frame = encode_state(bag, key, value, self.max_frame)?;
        self.post(Command::Broadcast { frame })
    }

    /// Tells every accepted player that the state bag `bag` was removed.
    pub fn broadcast_state_removed(&self, bag: &str) -> Result<(), NetError> {
        let frame = encode_state_removed(bag, self.max_frame)?;
        self.post(Command::Broadcast { frame })
    }

    /// Sends `Kicked` with `reason`, then closes the player's connection. The
    /// player's `Disconnected` event carries the same reason.
    pub fn kick(&self, player: u32, reason: &str) {
        self.post_lossy(Command::Kick {
            player,
            reason: reason.to_owned(),
        });
    }

    /// Closes all connections with a "server shutting down" reason and joins
    /// the network thread. Equivalent to dropping the handle.
    pub fn shutdown(self) {
        drop(self);
    }

    fn post(&self, command: Command) -> Result<(), NetError> {
        self.commands
            .as_ref()
            .ok_or(NetError::Closed)?
            .send(command)
            .map_err(|_| NetError::Closed)
    }

    fn post_lossy(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }
}

impl Drop for NetServer {
    fn drop(&mut self) {
        drop(self.commands.take());
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                error!("the network thread panicked");
            }
        }
    }
}

impl fmt::Debug for NetServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetServer")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

fn run_thread(
    bind: SocketAddr,
    credentials: Credentials,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedReceiver<Command>,
    ready: SyncSender<Result<SocketAddr, NetError>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = ready.send(Err(NetError::Io(err)));
            return;
        }
    };
    runtime.block_on(async move {
        let endpoint = match open_endpoint(bind, credentials) {
            Ok(endpoint) => endpoint,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        let local_addr = match endpoint.local_addr() {
            Ok(addr) => addr,
            Err(err) => {
                let _ = ready.send(Err(NetError::Io(err)));
                return;
            }
        };
        let _ = ready.send(Ok(local_addr));
        Supervisor::new(endpoint, shared).run(commands).await;
    });
}

fn open_endpoint(bind: SocketAddr, credentials: Credentials) -> Result<Endpoint, NetError> {
    let config = transport::server_config(credentials)?;
    Endpoint::server(config, bind).map_err(|source| NetError::Bind { addr: bind, source })
}

/// Bookkeeping for one connection, held by the supervisor.
struct Entry {
    task: Id,
    player: Option<u32>,
    decision: Option<oneshot::Sender<Decision>>,
    outbound: mpsc::Sender<Arc<[u8]>>,
    kick: Arc<watch::Sender<Option<Arc<str>>>>,
}

/// Owns every connection on the network thread.
struct Supervisor {
    endpoint: Endpoint,
    shared: Arc<Shared>,
    connections: HashMap<ConnectionId, Entry>,
    players: HashMap<u32, ConnectionId>,
    tasks: JoinSet<ConnectionId>,
    next_id: u64,
}

impl Supervisor {
    fn new(endpoint: Endpoint, shared: Arc<Shared>) -> Self {
        Self {
            endpoint,
            shared,
            connections: HashMap::new(),
            players: HashMap::new(),
            tasks: JoinSet::new(),
            next_id: 0,
        }
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle(command),
                    None => break,
                },
                incoming = self.endpoint.accept() => match incoming {
                    Some(incoming) => self.spawn_connection(incoming),
                    None => break,
                },
                Some(finished) = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    self.finished(finished);
                }
            }
        }
        self.shutdown().await;
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Accept { connection, player } => self.accept(connection, player),
            Command::Reject { connection, reason } => self.reject(connection, reason),
            Command::Send { player, frame } => {
                if let Some(&id) = self.players.get(&player) {
                    self.enqueue(id, frame);
                }
            }
            Command::Broadcast { frame } => {
                let ids: Vec<ConnectionId> = self.players.values().copied().collect();
                for id in ids {
                    self.enqueue(id, Arc::clone(&frame));
                }
            }
            Command::Kick { player, reason } => {
                if let Some(&id) = self.players.get(&player) {
                    self.kick_connection(id, &reason);
                }
            }
        }
    }

    fn spawn_connection(&mut self, incoming: Incoming) {
        let id = ConnectionId(self.next_id);
        self.next_id += 1;
        let (decision_tx, decision_rx) = oneshot::channel();
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_QUEUE_LEN);
        let (kick_tx, _) = watch::channel(None);
        let kick = Arc::new(kick_tx);
        let ticket = Ticket {
            id,
            decision: decision_rx,
            outbound: outbound_rx,
            kick: Arc::clone(&kick),
        };
        let shared = Arc::clone(&self.shared);
        let task = self
            .tasks
            .spawn(async move {
                connection::run(shared, incoming, ticket).await;
                id
            })
            .id();
        self.connections.insert(
            id,
            Entry {
                task,
                player: None,
                decision: Some(decision_tx),
                outbound: outbound_tx,
                kick,
            },
        );
    }

    fn accept(&mut self, id: ConnectionId, player: u32) {
        let Some(entry) = self.connections.get_mut(&id) else {
            debug!(connection = %id, "accept for an unknown connection");
            return;
        };
        let Some(decision) = entry.decision.take() else {
            debug!(connection = %id, "accept for a connection that is already decided");
            return;
        };
        if self.players.contains_key(&player) {
            warn!(
                player,
                "player id is already in use, rejecting the connection"
            );
            let _ = decision.send(Decision::Reject("player id already in use".to_owned()));
            return;
        }
        if decision.send(Decision::Accept(player)).is_ok() {
            entry.player = Some(player);
            self.players.insert(player, id);
        }
    }

    fn reject(&mut self, id: ConnectionId, reason: String) {
        let decision = self
            .connections
            .get_mut(&id)
            .and_then(|entry| entry.decision.take());
        if let Some(decision) = decision {
            let _ = decision.send(Decision::Reject(reason));
        } else {
            debug!(connection = %id, "reject for a connection that is already decided");
        }
    }

    fn enqueue(&mut self, id: ConnectionId, frame: Arc<[u8]>) {
        let Some(entry) = self.connections.get(&id) else {
            return;
        };
        if let Err(mpsc::error::TrySendError::Full(_)) = entry.outbound.try_send(frame) {
            warn!(connection = %id, "outbound queue is full, kicking the player");
            entry
                .kick
                .send_replace(Some(Arc::from(SEND_QUEUE_OVERFLOW)));
        }
    }

    fn kick_connection(&self, id: ConnectionId, reason: &str) {
        if let Some(entry) = self.connections.get(&id) {
            entry.kick.send_replace(Some(Arc::from(reason)));
        }
    }

    fn finished(&mut self, result: Result<ConnectionId, JoinError>) {
        let id = match result {
            Ok(id) => Some(id),
            Err(err) => {
                error!(%err, "connection task failed");
                self.connections
                    .iter()
                    .find(|(_, entry)| entry.task == err.id())
                    .map(|(id, _)| *id)
            }
        };
        let Some(id) = id else { return };
        if let Some(entry) = self.connections.remove(&id) {
            if let Some(player) = entry.player {
                if self.players.get(&player) == Some(&id) {
                    self.players.remove(&player);
                }
            }
        }
    }

    /// Kicks every player, drops the pending connections, waits briefly for the
    /// connection tasks to finish and closes the endpoint.
    async fn shutdown(mut self) {
        for entry in self.connections.values() {
            if entry.player.is_some() {
                entry.kick.send_replace(Some(Arc::from(SHUTTING_DOWN)));
            }
        }
        self.connections.clear();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        self.endpoint
            .close(VarInt::from_u32(0), SHUTTING_DOWN.as_bytes());
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, self.endpoint.wait_idle()).await;
    }
}
