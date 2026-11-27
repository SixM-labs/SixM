//! Blocking client for tests, bots and the game client.
//!
//! Each client runs a small tokio runtime on a dedicated thread. Frames are
//! read by a separate task, so that a partially read frame is never cancelled
//! by an outgoing command.

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::mpsc::{self as std_mpsc, Receiver, Sender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};
use ring::digest::{self, SHA256};
use sixm_core::Value;
use tokio::sync::mpsc;
use tracing::debug;

use crate::entities::EntityEntry;
use crate::error::NetError;
use crate::files::{FileHash, MAX_FILE_SIZE, ResourceEntry, hash_hex};
use crate::identity::Fingerprint;
use crate::protocol::{
    ClientMessage, MAX_FRAME, MAX_PLAYER_NAME_LEN, PROTOCOL_VERSION, SERVER_NAME, ServerMessage,
};
use crate::transfer::HEADER_LEN;
use crate::transport::{ServerVerifier, Trust, client_config, close_connection, closing_reason};
use crate::wire::{decode_server_message, encode_event, encode_frame, read_frame};

/// Time allowed for the connection to close after the client disconnects.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);

/// Something the server sent to this client.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The server sent an event.
    Event { name: String, args: Vec<Value> },
    /// A state bag key was set. `value` is nil when the key was deleted.
    StateBag {
        bag: String,
        key: String,
        value: Value,
    },
    /// A state bag was removed, because its player left.
    StateBagRemoved { bag: String },
    /// One frame of the resource snapshot sent on join. An empty list ends
    /// the snapshot. Entries of the same name in consecutive frames belong to
    /// one resource.
    Resources { entries: Vec<ResourceEntry> },
    /// A resource started. Its files replace any earlier entry of that name.
    ResourceStarted { entry: ResourceEntry },
    /// A resource stopped.
    ResourceStopped { name: String },
    /// An entity was created.
    EntityCreated { entity: EntityEntry },
    /// New positions and rotations, as `(id, position, rotation)`.
    EntityTransforms {
        updates: Vec<(u32, [f32; 3], [f32; 3])>,
    },
    /// An entity was deleted.
    EntityDeleted { id: u32 },
    /// The entities that existed when the client joined.
    EntitySnapshot { entities: Vec<EntityEntry> },
    /// The server kicked the client, or shut down, and gave a reason.
    Kicked { reason: String },
    /// The connection ended without a kick.
    Disconnected { reason: String },
}

/// Receivers waiting for a requested file, by hash. A waiter gets the file's
/// bytes, or `None` if the server does not advertise the file. A waiter whose
/// sender is dropped sees the connection end.
type Waiters = HashMap<FileHash, Vec<Sender<Option<Arc<Vec<u8>>>>>>;

/// The waiters of the file requests in flight, shared with the network thread.
type Pending = Arc<Mutex<Waiters>>;

/// Connection to a SixM server.
pub struct NetClient {
    player: u32,
    server_name: String,
    commands: Option<mpsc::UnboundedSender<Arc<[u8]>>>,
    events: Receiver<ClientEvent>,
    pending: Pending,
    thread: Option<JoinHandle<()>>,
}

/// Result of a successful handshake, reported back to the caller.
struct Welcome {
    player: u32,
    server_name: String,
}

impl NetClient {
    /// Connects to `addr` without checking the server's certificate, sends a
    /// `Hello` with `name` and waits for the server to accept or reject the
    /// connection. The connection is encrypted, but nothing shows that the
    /// server is the one the player meant to reach; use
    /// [`connect_pinned`](Self::connect_pinned) for that.
    ///
    /// Returns [`NetError::Rejected`] if the server refuses, and
    /// [`NetError::Timeout`] if the whole handshake takes longer than `timeout`.
    pub fn connect(addr: SocketAddr, name: &str, timeout: Duration) -> Result<Self, NetError> {
        Self::connect_with(addr, name, timeout, Trust::Any)
    }

    /// Like [`connect`](Self::connect), but accepts only a server whose
    /// certificate has the given fingerprint. Otherwise the handshake fails
    /// with [`NetError::CertificateMismatch`].
    pub fn connect_pinned(
        addr: SocketAddr,
        name: &str,
        fingerprint: &Fingerprint,
        timeout: Duration,
    ) -> Result<Self, NetError> {
        Self::connect_with(addr, name, timeout, Trust::Pinned(*fingerprint))
    }

    fn connect_with(
        addr: SocketAddr,
        name: &str,
        timeout: Duration,
        trust: Trust,
    ) -> Result<Self, NetError> {
        let name = name.trim().to_owned();
        if name.is_empty() || name.chars().count() > MAX_PLAYER_NAME_LEN {
            return Err(NetError::InvalidInput(format!(
                "player name must be 1 to {MAX_PLAYER_NAME_LEN} characters"
            )));
        }
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events_tx, events) = std_mpsc::channel();
        let (ready_tx, ready_rx) = sync_channel(1);
        let pending = Pending::default();
        let thread_pending = Arc::clone(&pending);
        let thread = thread::Builder::new()
            .name("sixm-client".to_owned())
            .spawn(move || {
                let link = Link {
                    ready: ready_tx,
                    commands: command_rx,
                    events: events_tx,
                    pending: thread_pending,
                };
                run_thread(addr, name, timeout, trust, link);
            })
            .map_err(NetError::Io)?;
        match ready_rx.recv() {
            Ok(Ok(welcome)) => Ok(Self {
                player: welcome.player,
                server_name: welcome.server_name,
                commands: Some(commands),
                events,
                pending,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(NetError::Closed)
            }
        }
    }

    /// Player id the server assigned to this client.
    pub fn player(&self) -> u32 {
        self.player
    }

    /// Name the server sent in its welcome.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Sends an event to the server.
    pub fn send_event(&self, name: &str, args: &[Value]) -> Result<(), NetError> {
        let frame = encode_event(ClientMessage::EVENT_TAG, name, args, MAX_FRAME)?;
        self.commands
            .as_ref()
            .ok_or(NetError::Closed)?
            .send(frame)
            .map_err(|_| NetError::Closed)
    }

    /// Waits up to `timeout` for the next event from the server. Returns `None`
    /// on timeout or once the connection has ended and every event was read.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<ClientEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Downloads the file with `hash` and checks it against `expected_size`
    /// and the hash. Blocks for at most `timeout` in total.
    ///
    /// Fails with [`NetError::FileUnavailable`] when the server does not
    /// advertise the file, and with [`NetError::Integrity`] when the bytes do
    /// not match the size or the hash. `expected_size` may not exceed
    /// [`MAX_FILE_SIZE`].
    pub fn download(
        &self,
        hash: FileHash,
        expected_size: u64,
        timeout: Duration,
    ) -> Result<Vec<u8>, NetError> {
        if expected_size > MAX_FILE_SIZE {
            return Err(NetError::InvalidInput(format!(
                "a file of {expected_size} bytes is over the {MAX_FILE_SIZE} byte limit"
            )));
        }
        let (waiter, delivered) = std_mpsc::channel();
        lock(&self.pending).entry(hash).or_default().push(waiter);
        let request = encode_frame(&ClientMessage::RequestFile { hash }, MAX_FRAME)?;
        self.commands
            .as_ref()
            .ok_or(NetError::Closed)?
            .send(request)
            .map_err(|_| NetError::Closed)?;
        let bytes = match delivered.recv_timeout(timeout) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return Err(NetError::FileUnavailable {
                    hash: hash_hex(&hash),
                });
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => return Err(NetError::Timeout),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return Err(NetError::Closed),
        };
        verify(&bytes, &hash, expected_size)?;
        Ok(Arc::unwrap_or_clone(bytes))
    }

    /// Closes the connection and waits for the client thread to finish.
    pub fn disconnect(self) {
        drop(self);
    }

    fn stop(&mut self) {
        drop(self.commands.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for NetClient {
    fn drop(&mut self) {
        self.stop();
    }
}

impl fmt::Debug for NetClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetClient")
            .field("player", &self.player)
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

/// Connection state after a successful handshake.
struct Established {
    endpoint: Endpoint,
    connection: Connection,
    send: SendStream,
    recv: RecvStream,
}

/// The channels that connect the caller's thread with the client thread.
struct Link {
    ready: SyncSender<Result<Welcome, NetError>>,
    commands: mpsc::UnboundedReceiver<Arc<[u8]>>,
    events: Sender<ClientEvent>,
    pending: Pending,
}

fn run_thread(addr: SocketAddr, name: String, timeout: Duration, trust: Trust, link: Link) {
    let Link {
        ready,
        commands,
        events,
        pending,
    } = link;
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
        let handshake = tokio::time::timeout(timeout, establish(addr, &name, trust)).await;
        let established = match handshake {
            Ok(Ok((established, welcome))) => {
                if ready.send(Ok(welcome)).is_err() {
                    return;
                }
                established
            }
            Ok(Err(err)) => {
                let _ = ready.send(Err(err));
                return;
            }
            Err(_) => {
                let _ = ready.send(Err(NetError::Timeout));
                return;
            }
        };
        session(established, commands, events, pending).await;
    });
}

/// Connects, sends the `Hello` and reads the server's first message.
async fn establish(
    addr: SocketAddr,
    name: &str,
    trust: Trust,
) -> Result<(Established, Welcome), NetError> {
    let bind = if addr.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let verifier = ServerVerifier::new(trust);
    let mut endpoint =
        Endpoint::client(bind).map_err(|source| NetError::Bind { addr: bind, source })?;
    endpoint.set_default_client_config(client_config(&verifier)?);
    let connecting = endpoint
        .connect(addr, SERVER_NAME)
        .map_err(|err| NetError::Connect(err.to_string()))?;
    let connection = connecting
        .await
        .map_err(|err| verifier.handshake_error(&err))?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|err| NetError::Connect(err.to_string()))?;
    let hello = encode_frame(
        &ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            name: name.to_owned(),
        },
        MAX_FRAME,
    )?;
    send.write_all(&hello)
        .await
        .map_err(|err| NetError::Connect(err.to_string()))?;
    let payload = read_frame(&mut recv, MAX_FRAME)
        .await?
        .ok_or(NetError::Closed)?;
    let welcome = match decode_server_message(&payload)? {
        ServerMessage::Welcome {
            player,
            server_name,
        } => Welcome {
            player,
            server_name,
        },
        ServerMessage::Rejected { reason } | ServerMessage::Kicked { reason } => {
            return Err(NetError::Rejected(reason));
        }
        ServerMessage::Event { .. }
        | ServerMessage::StateBag { .. }
        | ServerMessage::StateBagRemoved { .. }
        | ServerMessage::StateSnapshot { .. }
        | ServerMessage::Resources { .. }
        | ServerMessage::ResourceStarted { .. }
        | ServerMessage::ResourceStopped { .. }
        | ServerMessage::FileUnavailable { .. }
        | ServerMessage::EntityCreated { .. }
        | ServerMessage::EntityTransforms { .. }
        | ServerMessage::EntityDeleted { .. }
        | ServerMessage::EntitySnapshot { .. } => {
            return Err(NetError::Protocol(
                "the server sent a message before the welcome".to_owned(),
            ));
        }
    };
    Ok((
        Established {
            endpoint,
            connection,
            send,
            recv,
        },
        welcome,
    ))
}

/// Forwards outgoing frames and incoming events until either side ends. Files
/// the server sends on unidirectional streams are received alongside.
async fn session(
    established: Established,
    mut commands: mpsc::UnboundedReceiver<Arc<[u8]>>,
    events: Sender<ClientEvent>,
    pending: Pending,
) {
    let Established {
        endpoint,
        connection,
        mut send,
        recv,
        ..
    } = established;
    let mut reader = tokio::spawn(pump_events(
        recv,
        connection.clone(),
        events,
        Arc::clone(&pending),
    ));
    let files = tokio::spawn(accept_files(connection.clone(), Arc::clone(&pending)));
    loop {
        tokio::select! {
            outgoing = commands.recv() => match outgoing {
                Some(frame) => {
                    if send.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            _ = &mut reader => break,
        }
    }
    reader.abort();
    files.abort();
    // Waiters for files that will never arrive see the connection end.
    lock(&pending).clear();
    close_connection(&connection, 0, "client disconnected");
    let _ = tokio::time::timeout(DISCONNECT_GRACE, endpoint.wait_idle()).await;
}

/// Accepts the unidirectional streams that carry requested files, one task each.
async fn accept_files(connection: Connection, pending: Pending) {
    while let Ok(recv) = connection.accept_uni().await {
        tokio::spawn(receive_file(recv, Arc::clone(&pending)));
    }
}

/// Reads one file stream and hands its bytes to the callers waiting for that
/// hash. A stream nobody waits for is stopped. A stream that fails drops the
/// waiters, so they see the connection end.
async fn receive_file(mut recv: RecvStream, pending: Pending) {
    let mut header = [0_u8; HEADER_LEN];
    if recv.read_exact(&mut header).await.is_err() {
        return;
    }
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&header[..32]);
    let mut size_bytes = [0_u8; 8];
    size_bytes.copy_from_slice(&header[32..]);
    let size = u64::from_le_bytes(size_bytes);

    let waiters = lock(&pending).remove(&hash);
    let Some(waiters) = waiters else {
        let _ = recv.stop(VarInt::from_u32(0));
        return;
    };
    let Ok(len) = usize::try_from(size) else {
        return;
    };
    if size > MAX_FILE_SIZE {
        return;
    }
    let mut body = vec![0_u8; len];
    if recv.read_exact(&mut body).await.is_err() {
        return;
    }
    let bytes = Arc::new(body);
    for waiter in waiters {
        let _ = waiter.send(Some(Arc::clone(&bytes)));
    }
}

/// Checks downloaded bytes against the size and hash the server advertised.
fn verify(bytes: &[u8], hash: &FileHash, expected_size: u64) -> Result<(), NetError> {
    let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual_size != expected_size {
        return Err(NetError::Integrity {
            hash: hash_hex(hash),
            reason: format!("expected {expected_size} bytes, got {actual_size}"),
        });
    }
    if digest::digest(&SHA256, bytes).as_ref() != hash {
        return Err(NetError::Integrity {
            hash: hash_hex(hash),
            reason: "SHA-256 does not match".to_owned(),
        });
    }
    Ok(())
}

/// Locks the waiters. A poisoned lock still holds consistent data, since every
/// update is a single map operation, so the guard is used anyway.
fn lock(pending: &Pending) -> MutexGuard<'_, Waiters> {
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads server messages and reports them until the connection ends.
async fn pump_events(
    mut recv: RecvStream,
    connection: Connection,
    events: Sender<ClientEvent>,
    pending: Pending,
) {
    loop {
        let event = match read_frame(&mut recv, MAX_FRAME).await {
            Ok(Some(payload)) => match decode_server_message(&payload) {
                Ok(ServerMessage::Event { name, args }) => {
                    ClientEvent::Event { name, args: args.0 }
                }
                Ok(ServerMessage::StateBag { bag, key, value }) => ClientEvent::StateBag {
                    bag,
                    key,
                    value: value.0,
                },
                Ok(ServerMessage::StateBagRemoved { bag }) => ClientEvent::StateBagRemoved { bag },
                Ok(ServerMessage::Resources { entries }) => ClientEvent::Resources { entries },
                Ok(ServerMessage::ResourceStarted { entry }) => {
                    ClientEvent::ResourceStarted { entry }
                }
                Ok(ServerMessage::ResourceStopped { name }) => {
                    ClientEvent::ResourceStopped { name }
                }
                Ok(ServerMessage::FileUnavailable { hash }) => {
                    let waiters = lock(&pending).remove(&hash).unwrap_or_default();
                    for waiter in waiters {
                        let _ = waiter.send(None);
                    }
                    continue;
                }
                Ok(ServerMessage::StateSnapshot { entries }) => {
                    for (bag, key, value) in entries {
                        let event = ClientEvent::StateBag {
                            bag,
                            key,
                            value: value.0,
                        };
                        if events.send(event).is_err() {
                            debug!("the client handle was dropped, stopping the event reader");
                            return;
                        }
                    }
                    continue;
                }
                Ok(ServerMessage::EntityCreated { entity }) => {
                    ClientEvent::EntityCreated { entity }
                }
                Ok(ServerMessage::EntityTransforms { updates }) => {
                    ClientEvent::EntityTransforms { updates }
                }
                Ok(ServerMessage::EntityDeleted { id }) => ClientEvent::EntityDeleted { id },
                Ok(ServerMessage::EntitySnapshot { entities }) => {
                    ClientEvent::EntitySnapshot { entities }
                }
                Ok(ServerMessage::Kicked { reason }) => {
                    let _ = events.send(ClientEvent::Kicked { reason });
                    return;
                }
                Ok(ServerMessage::Welcome { .. } | ServerMessage::Rejected { .. }) => {
                    protocol_violation(&connection, &events, "unexpected handshake message");
                    return;
                }
                Err(err) => {
                    protocol_violation(&connection, &events, &err.to_string());
                    return;
                }
            },
            Ok(None) | Err(NetError::Closed) => {
                let reason = closing_reason(&connection).await;
                let _ = events.send(ClientEvent::Disconnected { reason });
                return;
            }
            Err(err) => {
                protocol_violation(&connection, &events, &err.to_string());
                return;
            }
        };
        if events.send(event).is_err() {
            debug!("the client handle was dropped, stopping the event reader");
            return;
        }
    }
}

fn protocol_violation(connection: &Connection, events: &Sender<ClientEvent>, reason: &str) {
    close_connection(connection, 1, reason);
    let _ = events.send(ClientEvent::Disconnected {
        reason: format!("protocol error: {reason}"),
    });
}
