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

use std::fs;
use std::path::{Path, PathBuf};

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use ring::digest;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sixm_core::{FuncRef, Value};
use sixm_net::{
    ClientEvent, ConnectionId, EntityEntry, FILE_KIND_CLIENT, FILE_KIND_FILE, FileEntry, FileHash,
    Fingerprint, Identity, MAX_EXPORT_NAME_LEN, MAX_FILE_SIZE, NetClient, NetError, NetEvent,
    NetServer, PROTOCOL_VERSION, PublishedFile, PublishedResource, ResourceEntry, ServerConfig,
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
    let handle =
        thread::spawn(move || NetClient::connect(addr, name, &Identity::Ephemeral, TIMEOUT));
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

/// A client endpoint that accepts any server certificate. It presents a
/// certificate of its own when `with_certificate` is set, and none otherwise.
fn raw_endpoint(with_certificate: bool) -> Endpoint {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 is supported")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny));
    let mut tls = if with_certificate {
        let certified =
            rcgen::generate_simple_self_signed(vec!["client".to_owned()]).expect("certificate");
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        builder
            .with_client_auth_cert(vec![certified.cert.der().clone()], key)
            .expect("client certificate is accepted")
    } else {
        builder.with_no_client_auth()
    };
    tls.alpn_protocols = vec![b"sixm/1".to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC config");
    let mut endpoint = Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0))).expect("endpoint");
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
    endpoint
}

async fn raw_connect(addr: SocketAddr) -> Raw {
    let endpoint = raw_endpoint(true);
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
    let handle =
        thread::spawn(move || NetClient::connect(addr, "bob", &Identity::Ephemeral, TIMEOUT));
    let (connection, name) = expect_connecting(&server);
    assert_eq!(name, "bob");
    server.net.reject(connection, "no room");
    match handle.join().expect("client thread does not panic") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "no room"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn pinned_client_connects_with_the_servers_fingerprint() {
    let server = start();
    let addr = server.net.local_addr();
    let fingerprint = server.net.fingerprint();
    let handle = thread::spawn(move || {
        NetClient::connect_pinned(addr, "carol", &fingerprint, &Identity::Ephemeral, TIMEOUT)
    });
    let (connection, _) = expect_connecting(&server);
    server.net.accept(connection, 3);
    let client = handle
        .join()
        .expect("client thread does not panic")
        .expect("pinned client connects");
    assert_eq!(client.player(), 3);
}

#[test]
fn pinned_client_refuses_another_servers_certificate() {
    let server = start();
    let addr = server.net.local_addr();
    let actual = server.net.fingerprint();
    let other = start();
    let expected = other.net.fingerprint();
    let handle = thread::spawn(move || {
        NetClient::connect_pinned(addr, "dave", &expected, &Identity::Ephemeral, TIMEOUT)
    });
    match handle.join().expect("client thread does not panic") {
        Err(NetError::CertificateMismatch {
            expected: pinned,
            actual: presented,
        }) => {
            assert_eq!(pinned, expected);
            assert_eq!(presented, actual);
        }
        unexpected => panic!("expected CertificateMismatch, got {unexpected:?}"),
    }
}

#[test]
fn client_rejects_invalid_names_before_connecting() {
    let server = start();
    let addr = server.net.local_addr();
    let long = "x".repeat(65);
    for name in ["", "   ", long.as_str()] {
        assert!(matches!(
            NetClient::connect(addr, name, &Identity::Ephemeral, TIMEOUT),
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

/// SHA-256 of `bytes`, as the file hash the protocol uses.
fn sha256(bytes: &[u8]) -> FileHash {
    let digest = digest::digest(&digest::SHA256, bytes);
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}

/// Writes `bytes` to `dir/name` and describes it as an advertised file.
fn published_file(dir: &Path, name: &str, kind: u8, bytes: &[u8]) -> PublishedFile {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("folder is created");
    }
    fs::write(&path, bytes).expect("file is written");
    PublishedFile {
        entry: FileEntry {
            name: name.to_owned(),
            kind,
            hash: sha256(bytes),
            size: u64::try_from(bytes.len()).expect("size fits"),
        },
        path,
    }
}

/// A resource with one file per `(name, contents)` pair.
fn resource(dir: &Path, name: &str, files: &[(&str, &[u8])]) -> PublishedResource {
    PublishedResource {
        name: name.to_owned(),
        files: files
            .iter()
            .map(|(file, bytes)| published_file(dir, file, FILE_KIND_CLIENT, bytes))
            .collect(),
        exports: Vec::new(),
    }
}

/// The wire form of a published resource.
fn entry_of(resource: &PublishedResource) -> ResourceEntry {
    ResourceEntry {
        name: resource.name.clone(),
        files: resource
            .files
            .iter()
            .map(|file| file.entry.clone())
            .collect(),
        exports: resource.exports.clone(),
    }
}

/// Bytes that vary, so that hashes differ between files.
fn patterned(len: usize, seed: u8) -> Vec<u8> {
    (0..251_u8)
        .cycle()
        .take(len)
        .map(|byte| byte.wrapping_add(seed))
        .collect()
}

/// Collects the snapshot that follows the welcome: frames until the empty one.
fn snapshot_of(client: &NetClient) -> Vec<ResourceEntry> {
    let mut merged: Vec<ResourceEntry> = Vec::new();
    loop {
        match client_event(client) {
            ClientEvent::Resources { entries } if entries.is_empty() => return merged,
            ClientEvent::Resources { entries } => {
                for entry in entries {
                    match merged.last_mut() {
                        Some(last) if last.name == entry.name => {
                            last.files.extend(entry.files);
                            last.exports.extend(entry.exports);
                        }
                        _ => merged.push(entry),
                    }
                }
            }
            other => panic!("expected the resource snapshot, got {other:?}"),
        }
    }
}

#[test]
fn snapshot_on_join_lists_every_resource_in_start_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = start();
    let names: Vec<String> = (0..600).map(|i| format!("f{i}.lua")).collect();
    let contents: Vec<Vec<u8>> = (0..600)
        .map(|i| patterned(8, u8::try_from(i % 200).expect("byte")))
        .collect();
    let big_refs: Vec<(&str, &[u8])> = names
        .iter()
        .zip(&contents)
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let mut second = resource(dir.path(), "second", &[("a.lua", b"a")]);
    second.exports = vec!["greet".to_owned(), "sum".to_owned()];
    let resources = vec![
        second,
        resource(dir.path(), "big", &big_refs),
        resource(dir.path(), "empty", &[]),
    ];
    let expected: Vec<ResourceEntry> = resources.iter().map(entry_of).collect();
    server
        .net
        .publish_resources(resources)
        .expect("resources are published");

    let client = join(&server, "tester", 1);
    server.net.send_resources(1).expect("snapshot is queued");
    assert_eq!(snapshot_of(&client), expected);
}

#[test]
fn resource_start_and_stop_are_broadcast_to_joined_players() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = start();
    let client = join(&server, "tester", 1);
    server.net.send_resources(1).expect("snapshot is queued");
    assert_eq!(snapshot_of(&client), Vec::new());

    let started = resource(dir.path(), "hello", &[("client/main.lua", b"print('hi')")]);
    let entry = entry_of(&started);
    server
        .net
        .publish_resources(vec![started])
        .expect("resources are published");
    server
        .net
        .broadcast_resource_started(entry.clone())
        .expect("broadcast is queued");
    assert_eq!(
        client_event(&client),
        ClientEvent::ResourceStarted { entry }
    );

    server
        .net
        .publish_resources(Vec::new())
        .expect("resources are published");
    server
        .net
        .broadcast_resource_stopped("hello")
        .expect("broadcast is queued");
    assert_eq!(
        client_event(&client),
        ClientEvent::ResourceStopped {
            name: "hello".to_owned()
        }
    );
}

#[test]
fn advertised_files_download_intact_including_five_mebibytes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = start();
    let small = patterned(100, 1);
    let large = patterned(5 * 1024 * 1024, 2);
    let small_file = published_file(dir.path(), "small.lua", FILE_KIND_FILE, &small);
    let large_file = published_file(dir.path(), "large.bin", FILE_KIND_FILE, &large);
    let small_entry = small_file.entry.clone();
    let large_entry = large_file.entry.clone();
    server
        .net
        .publish_resources(vec![PublishedResource {
            name: "files".to_owned(),
            files: vec![small_file, large_file],
            exports: Vec::new(),
        }])
        .expect("resources are published");

    let client = join(&server, "tester", 1);
    assert_eq!(
        client
            .download(small_entry.hash, small_entry.size, TIMEOUT)
            .expect("small file downloads"),
        small
    );
    assert_eq!(
        client
            .download(large_entry.hash, large_entry.size, TIMEOUT)
            .expect("large file downloads"),
        large
    );
}

#[test]
fn unknown_hash_is_reported_as_unavailable() {
    let server = start();
    let client = join(&server, "tester", 1);
    let result = client.download([0xab; 32], 10, TIMEOUT);
    assert!(
        matches!(result, Err(NetError::FileUnavailable { ref hash }) if hash.starts_with("abab")),
        "got {result:?}"
    );
}

#[test]
fn unpublished_file_becomes_unavailable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = start();
    let bytes = patterned(300, 4);
    let file = published_file(dir.path(), "gone.lua", FILE_KIND_FILE, &bytes);
    let (hash, size) = (file.entry.hash, file.entry.size);
    server
        .net
        .publish_resources(vec![PublishedResource {
            name: "gone".to_owned(),
            files: vec![file],
            exports: Vec::new(),
        }])
        .expect("resources are published");
    let client = join(&server, "tester", 1);
    assert_eq!(
        client
            .download(hash, size, TIMEOUT)
            .expect("file downloads"),
        bytes
    );

    server
        .net
        .publish_resources(Vec::new())
        .expect("resources are published");
    assert!(matches!(
        client.download(hash, size, TIMEOUT),
        Err(NetError::FileUnavailable { .. })
    ));
}

#[test]
fn file_changed_after_hashing_fails_verification() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = start();
    let original = patterned(2048, 5);
    let file = published_file(dir.path(), "edited.lua", FILE_KIND_FILE, &original);
    let (hash, size, path) = (file.entry.hash, file.entry.size, file.path.clone());
    server
        .net
        .publish_resources(vec![PublishedResource {
            name: "edited".to_owned(),
            files: vec![file],
            exports: Vec::new(),
        }])
        .expect("resources are published");
    fs::write(&path, patterned(2048, 6)).expect("file is changed");

    let client = join(&server, "tester", 1);
    assert!(matches!(
        client.download(hash, size, TIMEOUT),
        Err(NetError::Integrity { .. })
    ));
}

#[test]
fn invalid_publish_is_refused_and_the_previous_set_stays() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = start();
    let bytes = patterned(64, 7);
    let file = published_file(dir.path(), "kept.lua", FILE_KIND_FILE, &bytes);
    let (hash, size) = (file.entry.hash, file.entry.size);
    server
        .net
        .publish_resources(vec![PublishedResource {
            name: "kept".to_owned(),
            files: vec![file],
            exports: Vec::new(),
        }])
        .expect("resources are published");

    let relative = PublishedResource {
        name: "bad".to_owned(),
        files: vec![PublishedFile {
            entry: FileEntry {
                name: "x.lua".to_owned(),
                kind: FILE_KIND_FILE,
                hash: [1; 32],
                size: 1,
            },
            path: PathBuf::from("relative/x.lua"),
        }],
        exports: Vec::new(),
    };
    assert!(matches!(
        server.net.publish_resources(vec![relative]),
        Err(NetError::InvalidInput(_))
    ));

    let oversized = PublishedResource {
        name: "huge".to_owned(),
        files: vec![PublishedFile {
            entry: FileEntry {
                name: "huge.bin".to_owned(),
                kind: FILE_KIND_FILE,
                hash: [2; 32],
                size: MAX_FILE_SIZE + 1,
            },
            path: dir.path().join("huge.bin"),
        }],
        exports: Vec::new(),
    };
    assert!(matches!(
        server.net.publish_resources(vec![oversized]),
        Err(NetError::InvalidInput(_))
    ));

    let overlong_export = PublishedResource {
        name: "exports".to_owned(),
        files: Vec::new(),
        exports: vec!["x".repeat(MAX_EXPORT_NAME_LEN + 1)],
    };
    assert!(matches!(
        server.net.publish_resources(vec![overlong_export]),
        Err(NetError::InvalidInput(_))
    ));

    let client = join(&server, "tester", 1);
    assert_eq!(
        client.download(hash, size, TIMEOUT).expect("still served"),
        bytes
    );
}

#[test]
fn many_requests_on_one_connection_are_all_served() {
    // Twenty requests against eight transfer slots: the extra requests wait
    // for a slot, and every file still arrives.
    let dir = tempfile::tempdir().expect("temp dir");
    let server = start();
    let contents: Vec<Vec<u8>> = (0..20_u8).map(|i| patterned(200_000, i)).collect();
    let files: Vec<PublishedFile> = contents
        .iter()
        .enumerate()
        .map(|(i, bytes)| published_file(dir.path(), &format!("f{i}.bin"), FILE_KIND_FILE, bytes))
        .collect();
    let hashes: Vec<FileHash> = files.iter().map(|file| file.entry.hash).collect();
    server
        .net
        .publish_resources(vec![PublishedResource {
            name: "many".to_owned(),
            files,
            exports: Vec::new(),
        }])
        .expect("resources are published");

    let runtime = new_runtime();
    runtime.block_on(async {
        let (mut raw, id, _) = raw_hello(&server, "burst").await;
        server.net.accept(id, 1);
        server.net.send_resources(1).expect("snapshot is queued");
        // Welcome, then the snapshot: one frame of entries and the empty frame.
        for _ in 0..3 {
            read_payload(&mut raw.recv).await;
        }
        for hash in &hashes {
            let request = postcard::to_allocvec(&(2_u32, *hash)).expect("request encodes");
            raw.send
                .write_all(&frame(&request))
                .await
                .expect("request is written");
        }

        let mut received: Vec<(FileHash, Vec<u8>)> = Vec::new();
        for _ in 0..hashes.len() {
            let mut stream = tokio::time::timeout(TIMEOUT, raw.connection.accept_uni())
                .await
                .expect("a transfer starts in time")
                .expect("transfer stream opens");
            let mut header = [0_u8; 40];
            stream.read_exact(&mut header).await.expect("header");
            let mut hash = [0_u8; 32];
            hash.copy_from_slice(&header[..32]);
            let mut size = [0_u8; 8];
            size.copy_from_slice(&header[32..]);
            let len = usize::try_from(u64::from_le_bytes(size)).expect("size fits");
            let mut body = vec![0_u8; len];
            stream.read_exact(&mut body).await.expect("body");
            received.push((hash, body));
        }
        assert_eq!(received.len(), 20);
        for (hash, body) in &received {
            let index = hashes
                .iter()
                .position(|known| known == hash)
                .expect("advertised hash");
            assert_eq!(body, &contents[index]);
        }
    });
}

fn vehicle_entry(id: u32) -> EntityEntry {
    EntityEntry {
        id,
        kind: 2,
        model: 0xB779_A091,
        position: [1.0, 2.0, 3.0],
        rotation: [0.0, 0.0, 90.0],
    }
}

#[test]
fn entity_creation_transforms_and_deletion_arrive_in_order() {
    let server = start();
    let client = join(&server, "ann", 1);
    let entry = vehicle_entry(1);
    let moved = [(1, [4.0, 5.0, 6.0], [0.0, 0.0, 45.0])];

    server
        .net
        .broadcast_entity_created(entry)
        .expect("creation is sent");
    server
        .net
        .broadcast_entity_transforms(&moved)
        .expect("transforms are sent");
    server
        .net
        .broadcast_entity_deleted(1)
        .expect("deletion is sent");

    assert_eq!(
        client_event(&client),
        ClientEvent::EntityCreated { entity: entry }
    );
    assert_eq!(
        client_event(&client),
        ClientEvent::EntityTransforms {
            updates: moved.to_vec()
        }
    );
    assert_eq!(client_event(&client), ClientEvent::EntityDeleted { id: 1 });
}

#[test]
fn entity_snapshot_reaches_a_joining_player() {
    let server = start();
    let addr = server.net.local_addr();
    let handle =
        thread::spawn(move || NetClient::connect(addr, "ann", &Identity::Ephemeral, TIMEOUT));
    let (connection, _) = expect_connecting(&server);
    server.net.accept(connection, 1);
    let client = handle
        .join()
        .expect("client thread does not panic")
        .expect("client connects");

    let entities = [vehicle_entry(1), vehicle_entry(2)];
    server
        .net
        .send_entity_snapshot(1, &entities)
        .expect("snapshot is sent");

    assert_eq!(
        client_event(&client),
        ClientEvent::EntitySnapshot {
            entities: entities.to_vec()
        }
    );
}

/// Connects a client with `identity` on a background thread and accepts it as
/// `player`. Returns the client and the fingerprint the server verified.
fn join_as(server: &Server, identity: Identity, player: u32) -> (NetClient, Fingerprint) {
    let addr = server.net.local_addr();
    let handle = thread::spawn(move || NetClient::connect(addr, "ida", &identity, TIMEOUT));
    let (connection, verified) = match next_event(server) {
        NetEvent::Connecting {
            connection,
            fingerprint,
            ..
        } => (connection, fingerprint),
        other => panic!("expected Connecting, got {other:?}"),
    };
    server.net.accept(connection, player);
    let client = handle
        .join()
        .expect("client thread does not panic")
        .expect("client connects");
    (client, verified)
}

#[test]
fn persistent_identity_gives_the_same_fingerprint_on_every_connection() {
    let server = start();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join("identity.bin");

    let (first, first_seen) = join_as(&server, Identity::Persistent(path.clone()), 1);
    let (second, second_seen) = join_as(&server, Identity::Persistent(path), 2);
    assert_eq!(first_seen, second_seen);
    assert_eq!(first.fingerprint(), first_seen);
    assert_eq!(second.fingerprint(), second_seen);

    let (_fresh_a, ephemeral_a) = join_as(&server, Identity::Ephemeral, 3);
    let (_fresh_b, ephemeral_b) = join_as(&server, Identity::Ephemeral, 4);
    assert_ne!(ephemeral_a, ephemeral_b, "every ephemeral identity is new");
    assert_ne!(ephemeral_a, first_seen, "ephemeral and persistent differ");
}

#[test]
fn raw_client_without_a_certificate_cannot_get_a_welcome() {
    let server = start();
    let addr = server.net.local_addr();
    let runtime = new_runtime();
    let attempt = async {
        let endpoint = raw_endpoint(false);
        let Ok(connecting) = endpoint.connect(addr, "localhost") else {
            return Vec::new();
        };
        // TLS 1.3 lets the client finish first, so the refusal may come
        // later, while it reads the reply.
        let Ok(connection) = connecting.await else {
            return Vec::new();
        };
        let Ok((mut send, mut recv)) = connection.open_bi().await else {
            return Vec::new();
        };
        let hello = frame(&hello_payload(PROTOCOL_VERSION, "mallory"));
        let _ = send.write_all(&hello).await;
        recv.read_to_end(1 << 20).await.unwrap_or_default()
    };
    let reply = runtime
        .block_on(async { tokio::time::timeout(TIMEOUT, attempt).await })
        .expect("the attempt ends in time");

    assert!(reply.is_empty(), "the server sent {} bytes", reply.len());
    assert!(
        server.events.try_recv().is_err(),
        "the server reports no connection without a certificate"
    );
}
