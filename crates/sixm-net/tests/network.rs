//! End-to-end tests over loopback QUIC.
//!
//! Every test binds the server to `127.0.0.1:0`. Synchronisation uses channels
//! with timeouts, never sleeps. Raw connections speak the wire format directly
//! to check how the server handles malformed input.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sixm_core::{FuncRef, Value};
use sixm_net::{
    ClientEvent, ConnectionId, NetClient, NetError, NetEvent, NetServer, PROTOCOL_VERSION,
    ServerConfig,
};

const TIMEOUT: Duration = Duration::from_secs(5);

struct Server {
    net: NetServer,
    events: Receiver<NetEvent>,
}

fn start() -> Server {
    let (tx, events) = mpsc::channel();
    let config = ServerConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        server_name: "Test Server".to_owned(),
        ..ServerConfig::default()
    };
    let net = NetServer::bind(config, move |event| {
        let _ = tx.send(event);
    })
    .expect("server binds");
    Server { net, events }
}

fn next_event(server: &Server) -> NetEvent {
    server
        .events
        .recv_timeout(TIMEOUT)
        .expect("the server reports an event in time")
}

fn expect_connecting(server: &Server) -> (ConnectionId, String) {
    match next_event(server) {
        NetEvent::Connecting {
            connection, name, ..
        } => (connection, name),
        other => panic!("expected Connecting, got {other:?}"),
    }
}

fn expect_disconnected(server: &Server, player: u32) -> String {
    match next_event(server) {
        NetEvent::Disconnected {
            player: got,
            reason,
        } if got == player => reason,
        other => panic!("expected Disconnected for player {player}, got {other:?}"),
    }
}

fn client_event(client: &NetClient) -> ClientEvent {
    client
        .recv_timeout(TIMEOUT)
        .expect("the client receives an event in time")
}

/// Connects a client on a background thread, accepts it as `player` and
/// returns it once the handshake completes.
fn join(server: &Server, name: &'static str, player: u32) -> NetClient {
    let addr = server.net.local_addr();
    let handle = thread::spawn(move || NetClient::connect(addr, name, TIMEOUT));
    let (connection, _) = expect_connecting(server);
    server.net.accept(connection, player);
    handle
        .join()
        .expect("client thread does not panic")
        .expect("client connects")
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime builds")
}

/// Verifier for the raw test connections; the server certificate is not checked.
#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

/// A connection that speaks raw frames, for malformed-input tests.
struct Raw {
    _endpoint: Endpoint,
    connection: Connection,
    send: SendStream,
    recv: RecvStream,
}

async fn raw_connect(addr: SocketAddr) -> Raw {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 is supported")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"sixm/1".to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC config");
    let mut endpoint = Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0))).expect("endpoint");
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
    let connection = endpoint
        .connect(addr, "localhost")
        .expect("connect starts")
        .await
        .expect("QUIC handshake completes");
    let (send, recv) = connection.open_bi().await.expect("stream opens");
    Raw {
        _endpoint: endpoint,
        connection,
        send,
        recv,
    }
}

/// Prefixes a payload with its little-endian length.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(payload.len())
        .expect("payload fits in a frame")
        .to_le_bytes()
        .to_vec();
    out.extend_from_slice(payload);
    out
}

/// Encodes a `Hello` the way a client does: variant 0, protocol, name.
fn hello_payload(protocol: u32, name: &str) -> Vec<u8> {
    postcard::to_allocvec(&(0_u32, protocol, name)).expect("Hello encodes")
}

async fn read_payload(recv: &mut RecvStream) -> Vec<u8> {
    let mut header = [0_u8; 4];
    recv.read_exact(&mut header).await.expect("frame header");
    let len = usize::try_from(u32::from_le_bytes(header)).expect("frame length fits");
    let mut payload = vec![0_u8; len];
    recv.read_exact(&mut payload).await.expect("frame payload");
    payload
}

/// Opens a raw connection, sends a `Hello` with `name` and returns it together
/// with the `Connecting` event the server reports for it.
async fn raw_hello(server: &Server, name: &str) -> (Raw, ConnectionId, String) {
    let mut raw = raw_connect(server.net.local_addr()).await;
    raw.send
        .write_all(&frame(&hello_payload(PROTOCOL_VERSION, name)))
        .await
        .expect("Hello is written");
    let (id, received_name) = expect_connecting(server);
    (raw, id, received_name)
}

#[test]
fn accepted_client_receives_its_player_id_and_server_name() {
    let server = start();
    let client = join(&server, "alice", 7);
    assert_eq!(client.player(), 7);
    assert_eq!(client.server_name(), "Test Server");
}

#[test]
fn rejected_client_gets_the_reason() {
    let server = start();
    let addr = server.net.local_addr();
    let handle = thread::spawn(move || NetClient::connect(addr, "bob", TIMEOUT));
    let (connection, name) = expect_connecting(&server);
    assert_eq!(name, "bob");
    server.net.reject(connection, "no room");
    match handle.join().expect("client thread does not panic") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "no room"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn client_rejects_invalid_names_before_connecting() {
    let server = start();
    let addr = server.net.local_addr();
    let long = "x".repeat(65);
    for name in ["", "   ", long.as_str()] {
        assert!(matches!(
            NetClient::connect(addr, name, TIMEOUT),
            Err(NetError::InvalidInput(_))
        ));
    }
}

#[test]
fn server_trims_names_and_refuses_bad_names_and_protocol_mismatch() {
    let server = start();
    let runtime = new_runtime();
    runtime.block_on(async {
        let (padded, id, name) = raw_hello(&server, "  carol  ").await;
        assert_eq!(name, "carol");
        padded.connection.close(0_u32.into(), b"done");
        match next_event(&server) {
            NetEvent::Abandoned { connection } => assert_eq!(connection, id),
            other => panic!("expected Abandoned, got {other:?}"),
        }

        let long = "y".repeat(65);
        for (protocol, name) in [
            (PROTOCOL_VERSION, "   "),
            (PROTOCOL_VERSION, long.as_str()),
            (PROTOCOL_VERSION + 1, "dave"),
        ] {
            let mut raw = raw_connect(server.net.local_addr()).await;
            raw.send
                .write_all(&frame(&hello_payload(protocol, name)))
                .await
                .expect("Hello is written");
            let payload = read_payload(&mut raw.recv).await;
            let (tag, reason): (u32, String) =
                postcard::from_bytes(&payload).expect("Rejected decodes");
            assert_eq!(tag, 1, "the first server message is Rejected");
            assert_ne!(reason, "");
        }
    });
}

#[test]
fn kick_reaches_the_client_and_the_server_learns_the_reason() {
    let server = start();
    let client = join(&server, "erin", 3);
    server.net.kick(3, "bye");
    match client_event(&client) {
        ClientEvent::Kicked { reason } => assert_eq!(reason, "bye"),
        other => panic!("expected Kicked, got {other:?}"),
    }
    assert_eq!(expect_disconnected(&server, 3), "bye");
}

#[test]
fn client_disconnect_is_reported_to_the_server() {
    let server = start();
    let client = join(&server, "frank", 4);
    client.disconnect();
    assert_eq!(expect_disconnected(&server, 4), "client disconnected");
}

#[test]
fn leaving_before_accept_is_reported_as_abandoned() {
    let server = start();
    let runtime = new_runtime();
    runtime.block_on(async {
        let (raw, id, _) = raw_hello(&server, "gina").await;
        raw.connection.close(0_u32.into(), b"left");
        match next_event(&server) {
            NetEvent::Abandoned { connection } => assert_eq!(connection, id),
            other => panic!("expected Abandoned, got {other:?}"),
        }
    });
}

#[test]
fn sending_to_an_unknown_player_is_a_no_op() {
    let server = start();
    assert!(server.net.send_event(999, "nobody", &[]).is_ok());
    assert!(server.net.broadcast_event("nobody", &[]).is_ok());
}

#[test]
fn shutdown_kicks_every_player() {
    let server = start();
    let first = join(&server, "hana", 1);
    let second = join(&server, "ivan", 2);
    server.net.shutdown();
    for client in [&first, &second] {
        match client_event(client) {
            ClientEvent::Kicked { reason } => assert_eq!(reason, "server shutting down"),
            other => panic!("expected Kicked, got {other:?}"),
        }
    }
}

fn nested_values() -> Vec<Value> {
    vec![
        Value::Nil,
        Value::Bool(true),
        Value::Int(-42),
        Value::Float(1.5),
        Value::String(vec![0xff, 0xfe, 0x00, b'r', b'a', b'w'].into_boxed_slice()),
        Value::Array(vec![
            Value::Int(1),
            Value::Map(vec![(
                Value::from("key"),
                Value::Array(vec![Value::Float(-0.25), Value::Nil]),
            )]),
        ]),
        Value::Map(vec![(Value::from("a"), Value::Float(2.5))]),
    ]
}

#[test]
fn events_travel_both_ways_with_nested_values() {
    let server = start();
    let client = join(&server, "jo", 5);
    let args = nested_values();

    client.send_event("ping", &args).expect("event is queued");
    match next_event(&server) {
        NetEvent::Event {
            player,
            name,
            args: got,
        } => {
            assert_eq!(player, 5);
            assert_eq!(name, "ping");
            assert_eq!(got, args);
        }
        other => panic!("expected Event, got {other:?}"),
    }

    server
        .net
        .send_event(5, "pong", &args)
        .expect("event is queued");
    match client_event(&client) {
        ClientEvent::Event { name, args: got } => {
            assert_eq!(name, "pong");
            assert_eq!(got, args);
        }
        other => panic!("expected Event, got {other:?}"),
    }
}

#[test]
fn broadcast_reaches_every_player() {
    let server = start();
    let clients = [
        join(&server, "kim", 1),
        join(&server, "lee", 2),
        join(&server, "max", 3),
    ];
    let args = vec![Value::from("hello"), Value::Int(9)];
    server
        .net
        .broadcast_event("news", &args)
        .expect("broadcast is queued");
    for client in &clients {
        match client_event(client) {
            ClientEvent::Event { name, args: got } => {
                assert_eq!(name, "news");
                assert_eq!(got, args);
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }
}

#[test]
fn function_references_and_deep_values_are_not_encodable() {
    let server = start();
    let client = join(&server, "ned", 6);
    let func = Value::FuncRef(FuncRef {
        owner: "resource".into(),
        instance: 1,
        id: 1,
    });
    assert!(matches!(
        server.net.send_event(6, "f", std::slice::from_ref(&func)),
        Err(NetError::FunctionNotSendable)
    ));
    assert!(matches!(
        client.send_event("f", std::slice::from_ref(&func)),
        Err(NetError::FunctionNotSendable)
    ));
    assert!(matches!(
        server.net.broadcast_event("f", &[Value::Array(vec![func])]),
        Err(NetError::FunctionNotSendable)
    ));

    let mut deep = Value::Nil;
    for _ in 0..64 {
        deep = Value::Array(vec![deep]);
    }
    assert!(matches!(
        client.send_event("deep", &[deep]),
        Err(NetError::TooDeep)
    ));
    assert!(matches!(
        client.send_event("", &[]),
        Err(NetError::InvalidInput(_))
    ));
}

#[test]
fn excess_events_are_dropped_by_the_rate_limit() {
    let server = start();
    let client = join(&server, "oscar", 8);
    for index in 0..500 {
        client
            .send_event("spam", &[Value::Int(index)])
            .expect("event is queued");
    }
    let mut received = 0;
    while let Ok(event) = server.events.recv_timeout(Duration::from_secs(1)) {
        match event {
            NetEvent::Event { .. } => received += 1,
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(
        (200..500).contains(&received),
        "a burst of 200 plus refill is admitted, got {received}"
    );
}

#[test]
fn malformed_input_closes_only_the_offending_connection() {
    let server = start();
    let healthy = join(&server, "pat", 10);
    let runtime = new_runtime();

    let mut deep_payload = vec![1, 1, b'e', 1];
    for _ in 0..70 {
        deep_payload.extend_from_slice(&[5, 1]);
    }
    deep_payload.push(0);
    let cases: [(&str, Vec<u8>); 6] = [
        ("oversized frame", vec![0xff, 0xff, 0xff, 0x7f]),
        ("garbage payload", frame(&[0xff; 16])),
        ("deep nesting", frame(&deep_payload)),
        (
            "second Hello",
            frame(&hello_payload(PROTOCOL_VERSION, "again")),
        ),
        ("empty event name", frame(&[1, 0, 0])),
        ("trailing bytes", frame(&[1, 1, b'e', 0, 0xaa])),
    ];

    runtime.block_on(async {
        for (index, (case, bytes)) in cases.iter().enumerate() {
            let (mut raw, id, _) = raw_hello(&server, "victim").await;
            let player = 20 + u32::try_from(index).expect("small index");
            server.net.accept(id, player);
            let _ = raw.send.write_all(bytes).await;
            let reason = expect_disconnected(&server, player);
            assert!(
                reason.starts_with("protocol error"),
                "{case}: unexpected reason {reason}"
            );
            drop(raw);
        }
    });

    server
        .net
        .send_event(10, "still", &[])
        .expect("event is queued");
    match client_event(&healthy) {
        ClientEvent::Event { name, .. } => assert_eq!(name, "still"),
        other => panic!("expected Event, got {other:?}"),
    }
}

#[test]
fn player_who_stops_reading_is_kicked_when_its_queue_overflows() {
    let server = start();
    let runtime = new_runtime();
    runtime.block_on(async {
        let (_raw, id, _) = raw_hello(&server, "slow").await;
        server.net.accept(id, 30);
        let args = [Value::String(vec![7; 1024].into_boxed_slice())];
        for _ in 0..6000 {
            server
                .net
                .send_event(30, "bulk", &args)
                .expect("event is queued");
        }
        assert_eq!(expect_disconnected(&server, 30), "send queue overflow");
    });
}
