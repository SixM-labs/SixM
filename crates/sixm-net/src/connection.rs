//! Lifecycle of one QUIC connection: the handshake, then a reader and a
//! writer task for an accepted player.

use std::pin::pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use quinn::{Connection, Incoming, RecvStream, SendStream, VarInt};
use rustls::pki_types::CertificateDer;
use sixm_core::Value;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::entities::EntityTransform;
use crate::error::NetError;
use crate::files::FileHash;
use crate::identity::Fingerprint;
use crate::latent::{self, LatentJob};
use crate::protocol::{
    ClientMessage, EVENT_FLOOD, MAX_CONCURRENT_TRANSFERS, MAX_PENDING_REQUESTS,
    MAX_PLAYER_NAME_LEN, PROTOCOL_VERSION, SHUTTING_DOWN, ServerMessage, TOO_MANY_REQUESTS,
};
use crate::ratelimit::{RateLimiter, Verdict};
use crate::server::{ConnectionId, NetEvent};
use crate::traffic::Counters;
use crate::transfer::{Advertised, Transfer, serve_file};
use crate::transport::{close_connection, closing_reason};
use crate::wire::{
    FRAME_HEADER_LEN, decode_client_message, encode_frame, read_frame, validate_event_name,
};

/// Application close code for an ordinary end of the connection.
const CLOSE_NORMAL: u32 = 0;
/// Application close code for a protocol violation.
const CLOSE_PROTOCOL: u32 = 1;
/// Time allowed for a final frame to be acknowledged before closing.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Reason for closing a connection that presented no client certificate.
const NO_CLIENT_CERTIFICATE: &str = "a client certificate is required";

/// Settings and the event callback shared by every connection.
pub(crate) struct Shared {
    pub(crate) player_connections: crate::server::PlayerConnections,
    pub(crate) on_event: Arc<dyn Fn(NetEvent) + Send + Sync>,
    pub(crate) server_name: String,
    pub(crate) handshake_timeout: Duration,
    pub(crate) max_frame: usize,
    /// The files the server advertises, shared with the server handle.
    pub(crate) advertised: Arc<RwLock<Advertised>>,
    /// Traffic totals, shared with the server handle.
    pub(crate) traffic: Arc<Counters>,
}

/// The server's answer to a connection that completed its `Hello`.
pub(crate) enum Decision {
    Accept(u32),
    Reject(String),
}

/// What the server tells a connection that is still waiting for its decision.
pub(crate) enum Instruction {
    /// An encoded frame to send to the client, which keeps waiting.
    Update(Arc<[u8]>),
    /// The final answer. Nothing else is sent before it.
    Decide(Decision),
}

/// Channels that connect a connection task to the supervisor.
pub(crate) struct Ticket {
    pub(crate) id: ConnectionId,
    /// Updates and the decision. The task ends with the decision, or when the
    /// channel closes, which refuses the connection as shutting down.
    pub(crate) instructions: mpsc::UnboundedReceiver<Instruction>,
    pub(crate) outbound: mpsc::Receiver<Arc<[u8]>>,
    pub(crate) latent: mpsc::Receiver<LatentJob>,
    /// Another sender of the outbound queue, for replies the reader sends.
    pub(crate) reply: mpsc::Sender<Arc<[u8]>>,
    pub(crate) kick: Arc<watch::Sender<Option<Arc<str>>>>,
}

/// Drives one connection from its first packet until it ends.
pub(crate) async fn run(shared: Arc<Shared>, incoming: Incoming, ticket: Ticket) {
    let Ticket {
        id,
        mut instructions,
        outbound,
        latent,
        reply,
        kick,
    } = ticket;
    let connecting = match incoming.accept() {
        Ok(connecting) => connecting,
        Err(err) => {
            debug!(%err, "incoming connection failed before the handshake");
            return;
        }
    };
    let deadline = tokio::time::Instant::now() + shared.handshake_timeout;
    let connection = match tokio::time::timeout_at(deadline, connecting).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(err)) => {
            debug!(%err, "QUIC handshake failed");
            return;
        }
        Err(_) => {
            debug!("QUIC handshake timed out");
            return;
        }
    };
    let Some(fingerprint) = client_fingerprint(&connection) else {
        close_connection(&connection, CLOSE_PROTOCOL, NO_CLIENT_CERTIFICATE);
        return;
    };
    let hello = match tokio::time::timeout_at(
        deadline,
        read_hello(&connection, shared.max_frame, &shared.traffic),
    )
    .await
    {
        Ok(Ok(hello)) => hello,
        Ok(Err(NetError::Closed)) => return,
        Ok(Err(err)) => {
            close_connection(&connection, CLOSE_PROTOCOL, &err.to_string());
            return;
        }
        Err(_) => {
            close_connection(&connection, CLOSE_PROTOCOL, "handshake timed out");
            return;
        }
    };
    let Hello {
        mut send,
        recv,
        protocol,
        name,
    } = hello;
    if protocol != PROTOCOL_VERSION {
        let reason =
            format!("unsupported protocol version {protocol}, server speaks {PROTOCOL_VERSION}");
        return refuse(&connection, send, &reason, &shared).await;
    }
    let Some(name) = validate_player_name(&name) else {
        let reason = format!("player name must be 1 to {MAX_PLAYER_NAME_LEN} characters");
        return refuse(&connection, send, &reason, &shared).await;
    };
    shared
        .player_connections
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, connection.clone());
    (shared.on_event)(NetEvent::Connecting {
        connection: id,
        name,
        endpoint: connection.remote_address(),
        fingerprint,
    });

    let Some(decided) = decide(&shared, id, &connection, &mut send, &mut instructions).await else {
        return;
    };
    match decided {
        Decision::Reject(reason) => refuse(&connection, send, &reason, &shared).await,
        Decision::Accept(player) => {
            let accepted = Accepted {
                connection,
                player,
                send,
                recv,
                outbound,
                latent,
                reply,
                kick,
            };
            serve(&shared, accepted).await;
        }
    }
}

/// Waits for the server's decision on a connection that sent its `Hello`, and
/// writes every progress update to the client meanwhile. Returns `None` when
/// the connection ends first, after reporting it as abandoned.
async fn decide(
    shared: &Shared,
    id: ConnectionId,
    connection: &Connection,
    send: &mut SendStream,
    instructions: &mut mpsc::UnboundedReceiver<Instruction>,
) -> Option<Decision> {
    loop {
        tokio::select! {
            instruction = instructions.recv() => match instruction {
                Some(Instruction::Decide(decision)) => return Some(decision),
                Some(Instruction::Update(frame)) => {
                    if let Err(err) = send.write_all(&frame).await {
                        close_connection(
                            connection,
                            CLOSE_NORMAL,
                            &format!("deferral update failed: {err}"),
                        );
                        (shared.on_event)(NetEvent::Abandoned { connection: id });
                        return None;
                    }
                    shared.traffic.sent(frame.len());
                }
                None => return Some(Decision::Reject(SHUTTING_DOWN.to_owned())),
            },
            _ = connection.closed() => {
                (shared.on_event)(NetEvent::Abandoned { connection: id });
                return None;
            }
        }
    }
}

/// The fingerprint of the client certificate the handshake verified, or `None`
/// when the connection has no peer certificate.
fn client_fingerprint(connection: &Connection) -> Option<Fingerprint> {
    let identity = connection.peer_identity()?;
    let certificates = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certificates
        .first()
        .map(|certificate| Fingerprint::of(certificate))
}

/// Returns the trimmed player name if it is 1 to 64 characters long.
fn validate_player_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let len = trimmed.chars().count();
    (1..=MAX_PLAYER_NAME_LEN)
        .contains(&len)
        .then(|| trimmed.to_owned())
}

struct Hello {
    send: SendStream,
    recv: RecvStream,
    protocol: u32,
    name: String,
}

/// Opens the first bidirectional stream and reads the `Hello` on it.
async fn read_hello(
    connection: &Connection,
    max_frame: usize,
    traffic: &Counters,
) -> Result<Hello, NetError> {
    let (send, mut recv) = connection.accept_bi().await.map_err(|_| NetError::Closed)?;
    let payload = read_frame(&mut recv, max_frame)
        .await?
        .ok_or(NetError::Closed)?;
    traffic.received(FRAME_HEADER_LEN + payload.len());
    match decode_client_message(&payload)? {
        ClientMessage::Hello { protocol, name } => Ok(Hello {
            send,
            recv,
            protocol,
            name,
        }),
        ClientMessage::Event { .. }
        | ClientMessage::RequestFile { .. }
        | ClientMessage::Command { .. }
        | ClientMessage::EntityTransforms { .. } => Err(NetError::Protocol(
            "the first message must be Hello".to_owned(),
        )),
    }
}

/// Sends a `Rejected` message and closes the connection gracefully.
async fn refuse(connection: &Connection, mut send: SendStream, reason: &str, shared: &Shared) {
    let frame = encode_frame(
        &ServerMessage::Rejected {
            reason: reason.to_owned(),
        },
        shared.max_frame,
    )
    .ok();
    finish(&mut send, frame.as_deref(), &shared.traffic).await;
    close_connection(connection, CLOSE_NORMAL, reason);
}

/// Writes an optional last frame, finishes the stream and waits for the peer to
/// acknowledge it, so that the frame is not lost when the connection closes.
async fn finish(send: &mut SendStream, last: Option<&[u8]>, traffic: &Counters) {
    let flush = async {
        if let Some(bytes) = last
            && send.write_all(bytes).await.is_ok()
        {
            traffic.sent(bytes.len());
        }
        let _ = send.finish();
        let _ = send.stopped().await;
    };
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, flush).await;
}

/// An accepted connection, ready to start the reader and the writer.
struct Accepted {
    connection: Connection,
    player: u32,
    send: SendStream,
    recv: RecvStream,
    outbound: mpsc::Receiver<Arc<[u8]>>,
    latent: mpsc::Receiver<LatentJob>,
    reply: mpsc::Sender<Arc<[u8]>>,
    kick: Arc<watch::Sender<Option<Arc<str>>>>,
}

/// File requests of one player. Each request runs as a task that waits for a
/// transfer slot, and the tasks are dropped with the connection.
struct Requests {
    connection: Connection,
    advertised: Arc<RwLock<Advertised>>,
    slots: Arc<Semaphore>,
    reply: mpsc::Sender<Arc<[u8]>>,
    traffic: Arc<Counters>,
    tasks: JoinSet<()>,
}

impl Requests {
    fn new(
        connection: Connection,
        advertised: Arc<RwLock<Advertised>>,
        reply: mpsc::Sender<Arc<[u8]>>,
        traffic: Arc<Counters>,
    ) -> Self {
        Self {
            connection,
            advertised,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_TRANSFERS)),
            reply,
            traffic,
            tasks: JoinSet::new(),
        }
    }

    /// Starts serving the file with `hash`. Fails with the reason to kick the
    /// player when too many requests are still pending.
    fn request(&mut self, hash: FileHash) -> Result<(), &'static str> {
        while self.tasks.try_join_next().is_some() {}
        if self.tasks.len() >= MAX_PENDING_REQUESTS {
            return Err(TOO_MANY_REQUESTS);
        }
        let transfer = Transfer {
            connection: self.connection.clone(),
            advertised: Arc::clone(&self.advertised),
            slots: Arc::clone(&self.slots),
            reply: self.reply.clone(),
            traffic: Arc::clone(&self.traffic),
        };
        self.tasks.spawn(serve_file(transfer, hash));
        Ok(())
    }
}

/// Why the reader stopped.
enum Stop {
    /// Kick the player with this reason after the writer has said goodbye.
    Kick(String),
    /// The peer broke the protocol.
    Protocol(String),
    /// The stream or connection ended.
    Closed,
}

/// Runs an accepted player until the connection ends, then reports the
/// disconnect exactly once.
async fn serve(shared: &Shared, accepted: Accepted) {
    let Accepted {
        connection,
        player,
        send,
        mut recv,
        outbound,
        mut latent,
        reply,
        kick,
    } = accepted;
    let latent_connection = connection.clone();
    let latent_traffic = Arc::clone(&shared.traffic);
    let latent_sender = tokio::spawn(async move {
        let mut seq = 0_u64;
        while let Some(job) = latent.recv().await {
            if latent::send(&latent_connection, &mut seq, &job, Some(&latent_traffic))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let limiter = Arc::new(std::sync::Mutex::new(RateLimiter::new(Instant::now())));
    let latent_receiver = tokio::spawn(receive_latent(
        connection.clone(),
        player,
        Arc::clone(&shared.on_event),
        Arc::clone(&limiter),
        Arc::clone(&shared.traffic),
        kick.clone(),
    ));
    let mut writer = tokio::spawn(write_loop(WriteLoop {
        send,
        player,
        server_name: shared.server_name.clone(),
        outbound,
        kick: kick.subscribe(),
        max_frame: shared.max_frame,
        traffic: Arc::clone(&shared.traffic),
    }));
    let mut requests = Requests::new(
        connection.clone(),
        Arc::clone(&shared.advertised),
        reply,
        Arc::clone(&shared.traffic),
    );
    let mut reader = pin!(read_loop(shared, player, &mut recv, &mut requests, limiter));
    let (code, reason) = tokio::select! {
        stop = &mut reader => match stop {
            Stop::Kick(reason) => {
                kick.send_replace(Some(Arc::from(reason.as_str())));
                (CLOSE_NORMAL, join_writer(writer.await))
            }
            Stop::Protocol(reason) => {
                writer.abort();
                (CLOSE_PROTOCOL, format!("protocol error: {reason}"))
            }
            Stop::Closed => {
                let pending_kick = kick.borrow().clone();
                if pending_kick.is_some() {
                    (CLOSE_NORMAL, join_writer(writer.await))
                } else {
                    writer.abort();
                    (CLOSE_NORMAL, closing_reason(&connection).await)
                }
            }
        },
        ended = &mut writer => (CLOSE_NORMAL, join_writer(ended)),
    };
    close_connection(&connection, code, &reason);
    latent_sender.abort();
    latent_receiver.abort();
    (shared.on_event)(NetEvent::Disconnected { player, reason });
}

fn join_writer(result: Result<String, tokio::task::JoinError>) -> String {
    result.unwrap_or_else(|err| format!("connection task failed: {err}"))
}

/// Receives events, commands, entity transforms and file requests from the
/// player until the stream ends or the player is kicked for breaking a limit.
/// Events, commands and transform messages share one rate limit.
async fn read_loop(
    shared: &Shared,
    player: u32,
    recv: &mut RecvStream,
    requests: &mut Requests,
    limiter: Arc<std::sync::Mutex<RateLimiter>>,
) -> Stop {
    loop {
        let payload = match read_frame(recv, shared.max_frame).await {
            Ok(Some(payload)) => {
                shared.traffic.received(FRAME_HEADER_LEN + payload.len());
                payload
            }
            Ok(None) | Err(NetError::Closed) => return Stop::Closed,
            Err(err) => return Stop::Protocol(err.to_string()),
        };
        let inbound = match decode_client_message(&payload) {
            Ok(ClientMessage::Event { name, args }) => {
                if let Err(err) = validate_event_name(&name) {
                    return Stop::Protocol(err.to_string());
                }
                Inbound::Event { name, args: args.0 }
            }
            Ok(ClientMessage::Command { line }) => Inbound::Command { line },
            Ok(ClientMessage::EntityTransforms { updates }) => {
                Inbound::EntityTransforms { updates }
            }
            Ok(ClientMessage::RequestFile { hash }) => {
                if let Err(reason) = requests.request(hash) {
                    return Stop::Kick(reason.to_owned());
                }
                continue;
            }
            Ok(ClientMessage::Hello { .. }) => {
                return Stop::Protocol("Hello may only be sent once".to_owned());
            }
            Err(err) => return Stop::Protocol(err.to_string()),
        };
        let now = Instant::now();
        let verdict = limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admit(now);
        match verdict {
            Verdict::Accept => (shared.on_event)(match inbound {
                Inbound::Event { name, args } => NetEvent::Event { player, name, args },
                Inbound::Command { line } => NetEvent::Command { player, line },
                Inbound::EntityTransforms { updates } => {
                    NetEvent::EntityTransforms { player, updates }
                }
            }),
            Verdict::Drop => {
                if let Some(dropped) = limiter
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take_drop_report(now)
                {
                    warn!(player, dropped, "dropping events above the rate limit");
                }
            }
            Verdict::Flood => return Stop::Kick(EVENT_FLOOD.to_owned()),
        }
    }
}

/// Accepts latent streams concurrently and emits their events in sequence order.
async fn receive_latent(
    connection: Connection,
    player: u32,
    on_event: Arc<dyn Fn(NetEvent) + Send + Sync>,
    limiter: Arc<std::sync::Mutex<RateLimiter>>,
    traffic: Arc<Counters>,
    kick: Arc<watch::Sender<Option<Arc<str>>>>,
) {
    let slots = Arc::new(Semaphore::new(crate::protocol::MAX_LATENT_STREAMS));
    let mut tasks = JoinSet::new();
    let mut sequence = latent::Sequencer::default();
    loop {
        tokio::select! {
            incoming = connection.accept_uni(), if tasks.len() < crate::protocol::MAX_LATENT_STREAMS => match incoming {
                Ok(stream) => {
                    let slots = Arc::clone(&slots);
                    let traffic = Arc::clone(&traffic);
                    tasks.spawn(receive_latent_stream(
                        stream,
                        slots,
                        traffic,
                        connection.clone(),
                    ));
                }
                Err(_) => break,
            },
            Some(result) = tasks.join_next(), if !tasks.is_empty() => if let Ok(Some(arrival)) = result {
                match sequence.push(arrival) {
                    Ok(ready) => for event in ready {
                        let now = Instant::now();
                        let verdict = limiter.lock().unwrap_or_else(std::sync::PoisonError::into_inner).admit(now);
                        match verdict {
                            Verdict::Accept => (on_event)(NetEvent::Event { player, name: event.name, args: event.args }),
                            Verdict::Drop => {
                                if let Some(dropped) = limiter.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take_drop_report(now) {
                                    warn!(player, dropped, "dropping events above the rate limit");
                                }
                            },
                            Verdict::Flood => { kick.send_replace(Some(Arc::from(EVENT_FLOOD))); return; }
                        }
                    },
                    Err(err) => { close_connection(&connection, CLOSE_PROTOCOL, &err.to_string()); break; }
                }
            },
        }
    }
    tasks.abort_all();
}

async fn receive_latent_stream(
    mut stream: RecvStream,
    slots: Arc<Semaphore>,
    traffic: Arc<Counters>,
    connection: Connection,
) -> Option<latent::Arrival> {
    match latent::read_kind(&mut stream).await {
        Some(crate::protocol::STREAM_KIND_LATENT) => {
            let arrival = latent::receive(stream, slots, Some(traffic)).await;
            if arrival.is_none() {
                close_connection(
                    &connection,
                    CLOSE_PROTOCOL,
                    "latent stream ended before its sequence header",
                );
            }
            arrival
        }
        Some(_) => {
            close_connection(
                &connection,
                CLOSE_PROTOCOL,
                "unexpected client unidirectional stream",
            );
            None
        }
        None => {
            close_connection(
                &connection,
                CLOSE_PROTOCOL,
                "latent stream ended before its kind byte",
            );
            None
        }
    }
}

/// A decoded message from a player that is delivered if the rate limit admits it.
enum Inbound {
    Event { name: String, args: Vec<Value> },
    Command { line: String },
    EntityTransforms { updates: Vec<EntityTransform> },
}

/// Everything the writer task owns.
struct WriteLoop {
    send: SendStream,
    player: u32,
    server_name: String,
    outbound: mpsc::Receiver<Arc<[u8]>>,
    kick: watch::Receiver<Option<Arc<str>>>,
    max_frame: usize,
    traffic: Arc<Counters>,
}

/// Sends the welcome, then queued frames, until the player is kicked or the
/// queue closes. Returns the reason the writer stopped.
async fn write_loop(writer: WriteLoop) -> String {
    let WriteLoop {
        mut send,
        player,
        server_name,
        mut outbound,
        mut kick,
        max_frame,
        traffic,
    } = writer;
    let welcome = match encode_frame(
        &ServerMessage::Welcome {
            player,
            server_name,
        },
        max_frame,
    ) {
        Ok(frame) => frame,
        Err(err) => return err.to_string(),
    };
    if let Err(reason) = write_frame(&mut send, &welcome, &mut kick, &traffic).await {
        return reason;
    }
    loop {
        let frame = tokio::select! {
            biased;
            reason = next_kick(&mut kick) => return kicked(&mut send, &reason, max_frame, &traffic).await,
            frame = outbound.recv() => {
                let Some(frame) = frame else {
                    finish(&mut send, None, &traffic).await;
                    return SHUTTING_DOWN.to_owned();
                };
                frame
            }
        };
        if let Err(reason) = write_frame(&mut send, &frame, &mut kick, &traffic).await {
            return reason;
        }
    }
}

/// Writes one frame. A kick that arrives while the write is blocked, which is
/// what happens to a client that stopped reading, resets the stream instead,
/// and its reason is returned as the error.
async fn write_frame(
    send: &mut SendStream,
    bytes: &[u8],
    kick: &mut watch::Receiver<Option<Arc<str>>>,
    traffic: &Counters,
) -> Result<(), String> {
    tokio::select! {
        biased;
        reason = next_kick(kick) => {
            let _ = send.reset(VarInt::from_u32(CLOSE_NORMAL));
            Err(reason.to_string())
        }
        result = send.write_all(bytes) => match result {
            Ok(()) => {
                traffic.sent(bytes.len());
                Ok(())
            }
            Err(err) => Err(format!("write failed: {err}")),
        },
    }
}

/// Waits until the player is kicked and returns the reason.
async fn next_kick(kick: &mut watch::Receiver<Option<Arc<str>>>) -> Arc<str> {
    loop {
        if kick.changed().await.is_err() {
            return std::future::pending().await;
        }
        if let Some(reason) = kick.borrow_and_update().clone() {
            return reason;
        }
    }
}

/// Sends `Kicked` with `reason`, finishes the stream and returns the reason.
async fn kicked(
    send: &mut SendStream,
    reason: &str,
    max_frame: usize,
    traffic: &Counters,
) -> String {
    let frame = encode_frame(
        &ServerMessage::Kicked {
            reason: reason.to_owned(),
        },
        max_frame,
    )
    .ok();
    finish(send, frame.as_deref(), traffic).await;
    reason.to_owned()
}
