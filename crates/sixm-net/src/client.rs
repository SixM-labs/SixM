//! Blocking client for tests, bots and the game client.
//!
//! Each client runs a small tokio runtime on a dedicated thread. Frames are
//! read by a separate task, so that a partially read frame is never cancelled
//! by an outgoing command.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::mpsc::{self as std_mpsc, Receiver, Sender, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use sixm_core::Value;
use tokio::sync::mpsc;
use tracing::debug;

use crate::error::NetError;
use crate::identity::Fingerprint;
use crate::protocol::{
    ClientMessage, MAX_FRAME, MAX_PLAYER_NAME_LEN, PROTOCOL_VERSION, SERVER_NAME, ServerMessage,
};
use crate::transport::{ServerVerifier, Trust, client_config, close_connection, closing_reason};
use crate::wire::{decode_server_message, encode_event, encode_frame, read_frame};

/// Time allowed for the connection to close after the client disconnects.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);

/// Something the server sent to this client.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The server sent an event.
    Event { name: String, args: Vec<Value> },
    /// The server kicked the client, or shut down, and gave a reason.
    Kicked { reason: String },
    /// The connection ended without a kick.
    Disconnected { reason: String },
}

/// Connection to a SixM server.
pub struct NetClient {
    player: u32,
    server_name: String,
    commands: Option<mpsc::UnboundedSender<Arc<[u8]>>>,
    events: Receiver<ClientEvent>,
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
        let thread = thread::Builder::new()
            .name("sixm-client".to_owned())
            .spawn(move || {
                run_thread(addr, name, timeout, trust, ready_tx, command_rx, events_tx);
            })
            .map_err(NetError::Io)?;
        match ready_rx.recv() {
            Ok(Ok(welcome)) => Ok(Self {
                player: welcome.player,
                server_name: welcome.server_name,
                commands: Some(commands),
                events,
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

fn run_thread(
    addr: SocketAddr,
    name: String,
    timeout: Duration,
    trust: Trust,
    ready: SyncSender<Result<Welcome, NetError>>,
    commands: mpsc::UnboundedReceiver<Arc<[u8]>>,
    events: Sender<ClientEvent>,
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
        session(established, commands, events).await;
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
        ServerMessage::Event { .. } => {
            return Err(NetError::Protocol(
                "the server sent an event before the welcome".to_owned(),
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

/// Forwards outgoing frames and incoming events until either side ends.
async fn session(
    established: Established,
    mut commands: mpsc::UnboundedReceiver<Arc<[u8]>>,
    events: Sender<ClientEvent>,
) {
    let Established {
        endpoint,
        connection,
        mut send,
        recv,
        ..
    } = established;
    let mut reader = tokio::spawn(pump_events(recv, connection.clone(), events));
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
    close_connection(&connection, 0, "client disconnected");
    let _ = tokio::time::timeout(DISCONNECT_GRACE, endpoint.wait_idle()).await;
}

/// Reads server messages and reports them until the connection ends.
async fn pump_events(mut recv: RecvStream, connection: Connection, events: Sender<ClientEvent>) {
    loop {
        let event = match read_frame(&mut recv, MAX_FRAME).await {
            Ok(Some(payload)) => match decode_server_message(&payload) {
                Ok(ServerMessage::Event { name, args }) => {
                    ClientEvent::Event { name, args: args.0 }
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
