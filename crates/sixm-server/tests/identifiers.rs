//! Player identifiers end to end: real loopback clients, and a script that
//! records what `playerConnecting` and `playerJoining` can see.

mod common;

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_core::{Host, Value};
use sixm_net::{Identity, NetClient, NetError};
use sixm_server::Input;
use tempfile::TempDir;

/// Longest a test waits for something the server should do.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long each wait step blocks before pumping the server again.
const STEP: Duration = Duration::from_millis(5);

/// The `ids` resource: notes what each connecting and joining player sees.
const IDS_SCRIPT: &str = r"
LOG = {}

local function note(entry)
    LOG[#LOG + 1] = entry
end

AddEventHandler('playerConnecting', function()
    note(('connecting %d name=%s license=%s players=[%s]'):format(
        source,
        GetPlayerName(source),
        GetPlayerIdentifierByType(source, 'license') or '-',
        table.concat(GetPlayers(), ',')))
end)

AddEventHandler('playerJoining', function()
    note(('joining %d name=%s identifiers=%s'):format(
        source,
        GetPlayerName(source),
        table.concat(GetPlayerIdentifiers(source), ' ')))
end)

function log()
    return table.concat(LOG, '\n')
end

function online(id)
    return GetPlayerTimeOnline(id)
end

function guid(id)
    return GetPlayerGuid(id)
end

function host_id()
    return GetHostId()
end
";

/// A server on loopback with the `ids` resource running.
struct Harness {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
}

impl Harness {
    fn new() -> Self {
        let fx = Fixture::new();
        fx.add_resource(
            "ids",
            &manifest(&["server.lua"], &["log", "online", "guid", "host_id"], &[]),
            &[("server.lua", IDS_SCRIPT)],
        );
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        fx.server.execute("ensure ids");
        Self { fx, input, addr }
    }

    /// Handles every queued network input and runs one frame.
    fn pump(&self) {
        while let Ok(input) = self.input.try_recv() {
            self.fx.server.handle(input);
        }
        self.fx.server.tick(Instant::now());
    }

    /// Connects a client named `name` with `identity`, pumping the server during
    /// the handshake.
    fn connect(&self, name: &str, identity: &Identity) -> Result<NetClient, NetError> {
        let addr = self.addr;
        let name = name.to_owned();
        let identity = identity.clone();
        let handshake = thread::spawn(move || NetClient::connect(addr, &name, &identity, TIMEOUT));
        loop {
            self.pump();
            if handshake.is_finished() {
                return handshake.join().expect("the client thread does not panic");
            }
            thread::sleep(STEP);
        }
    }

    /// The `ids` log as one string.
    fn log(&self) -> String {
        self.fx.text("ids", "log")
    }

    /// Waits until the log contains `entry`.
    fn wait_for_log(&self, entry: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if self.log().contains(entry) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for log entry `{entry}`; log: {}",
                self.log()
            );
            thread::sleep(STEP);
        }
    }
}

/// The `license` identifier the server must derive from the client's certificate.
fn license(client: &NetClient) -> String {
    format!("license:{}", client.fingerprint())
}

#[test]
fn license_comes_from_the_client_identity_and_ip_from_the_endpoint() {
    let h = Harness::new();
    let dir = TempDir::new().expect("temp dir");
    let identity = Identity::Persistent(dir.path().join("identity.bin"));

    let alice = h.connect("Alice", &identity).expect("Alice is admitted");
    let alice_license = license(&alice);
    h.wait_for_log(&format!(
        "joining 1 name=Alice identifiers={alice_license} ip:127.0.0.1"
    ));
    h.wait_for_log(&format!(
        "connecting 1 name=Alice license={alice_license} players=[]"
    ));

    let bob = h.connect("Bob", &identity).expect("Bob is admitted");
    assert_eq!(
        license(&bob),
        alice_license,
        "the same file gives the same license"
    );
    h.wait_for_log(&format!(
        "connecting 2 name=Bob license={alice_license} players=[1]"
    ));
    h.wait_for_log(&format!(
        "joining 2 name=Bob identifiers={alice_license} ip:127.0.0.1"
    ));

    let carol = h
        .connect("Carol", &Identity::Ephemeral)
        .expect("Carol is admitted");
    let carol_license = license(&carol);
    assert_ne!(
        carol_license, alice_license,
        "another identity is another license"
    );
    h.wait_for_log(&format!(
        "joining 3 name=Carol identifiers={carol_license} ip:127.0.0.1"
    ));
}

#[test]
fn time_online_guid_and_host_id_for_a_connected_player() {
    let h = Harness::new();
    let alice = h
        .connect("Alice", &Identity::Ephemeral)
        .expect("Alice is admitted");
    let alice_license = license(&alice);
    h.wait_for_log(&format!(
        "joining 1 name=Alice identifiers={alice_license} ip:127.0.0.1"
    ));

    let manager = h.fx.manager();
    match manager
        .call_export("ids", "online", &[Value::Int(1)], None)
        .expect("online is callable")
        .as_slice()
    {
        [Value::Int(seconds)] => assert!(
            (0..=5).contains(seconds),
            "a fresh connection is a few seconds old, got {seconds}"
        ),
        other => panic!("unexpected online result {other:?}"),
    }
    let hex = alice_license
        .strip_prefix("license:")
        .expect("the license identifier has the prefix");
    assert_eq!(
        manager
            .call_export("ids", "guid", &[Value::Int(1)], None)
            .expect("guid is callable"),
        vec![Value::from(hex)]
    );
    assert_eq!(
        manager
            .call_export("ids", "host_id", &[], None)
            .expect("host_id is callable"),
        vec![Value::from("1")]
    );
}
