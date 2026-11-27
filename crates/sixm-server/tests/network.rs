//! Networking end to end: a server listening on loopback, real `NetClient`s and
//! a script that reacts to players.
//!
//! The test thread plays the server loop. It pumps the server's input and ticks
//! it while it waits for clients, because `NetClient::connect` and
//! `recv_timeout` both block.

mod common;

use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_core::{Host, Value};
use sixm_net::{ClientEvent, Identity, NetClient, NetError};
use sixm_server::Input;

/// Longest a test waits for something the server should do.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long each wait step blocks before pumping the server again.
const STEP: Duration = Duration::from_millis(5);

/// The `net` resource: records what happened in `LOG` and exposes it.
const NET_SCRIPT: &str = r"
LOG = {}
SAVED = nil

local function note(entry)
    LOG[#LOG + 1] = entry
end

AddEventHandler('playerConnecting', function(name, setKickReason)
    note('connecting:' .. name)
    SAVED = setKickReason
    if name == 'reason' then
        setKickReason('no entry for reason')
    elseif name == 'cancel' then
        CancelEvent()
    end
end)

AddEventHandler('playerJoining', function()
    note('joined:' .. source .. ':' .. GetPlayerName(source) .. ':' .. #GetPlayers())
end)

AddEventHandler('playerDropped', function(reason)
    note('dropped:' .. source .. ':' .. reason)
end)

AddEventHandler('net:secret', function(text)
    note('secret:' .. text)
end)

RegisterNetEvent('net:ping', function(text)
    TriggerClientEvent('net:pong', source, 'pong: ' .. text)
end)

function log()
    return table.concat(LOG, ' ')
end

function names()
    local out = {}
    for _, id in ipairs(GetPlayers()) do
        out[#out + 1] = id .. '=' .. GetPlayerName(id)
    end
    return table.concat(out, ',')
end

function broadcast(text)
    TriggerClientEvent('net:all', -1, text)
end

function kick(id, reason)
    DropPlayer(id, reason)
end

function bad_send()
    local ok, err = pcall(TriggerClientEvent, 'net:all', -1, function() end)
    return tostring(ok) .. ' ' .. tostring(err)
end

function late()
    local ok, err = pcall(SAVED, 'too late')
    return tostring(ok) .. ' ' .. tostring(err)
end
";

/// A server on loopback with the `net` resource running.
struct Harness {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
}

impl Harness {
    fn new() -> Self {
        let fx = Fixture::new();
        fx.add_resource(
            "net",
            &manifest(
                &["server.lua"],
                &["log", "names", "broadcast", "kick", "bad_send", "late"],
                &[],
            ),
            &[("server.lua", NET_SCRIPT)],
        );
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        fx.server.execute("ensure net");
        Self { fx, input, addr }
    }

    /// Handles every queued network input and runs one frame.
    fn pump(&self) {
        while let Ok(input) = self.input.try_recv() {
            self.fx.server.handle(input);
        }
        self.fx.server.tick(Instant::now());
    }

    /// Pumps the server until `check` returns a value, or fails after [`TIMEOUT`].
    fn wait_for<T>(&self, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(STEP);
        }
    }

    /// Connects a client named `name`, pumping the server during the handshake.
    fn connect(&self, name: &str) -> Result<NetClient, NetError> {
        let addr = self.addr;
        let name = name.to_owned();
        let handshake =
            thread::spawn(move || NetClient::connect(addr, &name, &Identity::Ephemeral, TIMEOUT));
        loop {
            self.pump();
            if handshake.is_finished() {
                return handshake.join().expect("the client thread does not panic");
            }
            thread::sleep(STEP);
        }
    }

    /// Waits for the next event named `name` on `client` and returns its arguments.
    fn expect_event(&self, client: &NetClient, name: &str) -> Vec<Value> {
        self.wait_for(&format!("event {name}"), || {
            match client.recv_timeout(STEP) {
                Some(ClientEvent::Event {
                    name: received,
                    args,
                }) if received == name => Some(args),
                _ => None,
            }
        })
    }

    /// Calls `export` of `net` with `args`, discarding its results.
    fn call(&self, export: &str, args: &[Value]) {
        self.fx
            .manager()
            .call_export("net", export, args, None)
            .unwrap_or_else(|err| panic!("export `{export}`: {err}"));
    }

    /// The `net` log as one string.
    fn log(&self) -> String {
        self.fx.text("net", "log")
    }

    /// Waits until the `net` log contains `entry`.
    fn wait_for_log(&self, entry: &str) {
        self.wait_for(&format!("log entry `{entry}`"), || {
            self.log().contains(entry).then_some(())
        });
    }
}

#[test]
fn joining_player_runs_playerjoining_and_is_listed() {
    let h = Harness::new();
    let alice = h.connect("Alice").expect("Alice is admitted");

    assert_eq!(alice.player(), 1);
    assert_eq!(alice.server_name(), "SixM Server");
    assert!(h.log().contains("connecting:Alice"), "log: {}", h.log());
    assert!(h.log().contains("joined:1:Alice:1"), "log: {}", h.log());
    assert_eq!(h.fx.text("net", "names"), "1=Alice");
}

#[test]
fn set_kick_reason_refuses_the_connection() {
    let h = Harness::new();
    match h.connect("reason") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "no entry for reason"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the connection should be refused"),
    }
    assert!(!h.log().contains("joined"), "log: {}", h.log());
}

#[test]
fn cancel_event_refuses_with_default_reason() {
    let h = Harness::new();
    match h.connect("cancel") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "Rejected by the server"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the connection should be refused"),
    }
}

#[test]
fn full_server_refuses_extra_players() {
    let h = Harness::new();
    h.fx.server.execute("set sv_maxclients 1");
    let _first = h.connect("First").expect("the first player fits");
    match h.connect("Second") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "Server is full"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the second player should be refused"),
    }
}

#[test]
fn net_event_round_trip_reaches_the_handler_and_back() {
    let h = Harness::new();
    let alice = h.connect("Alice").expect("Alice is admitted");
    alice
        .send_event("net:ping", &[Value::from("hi")])
        .expect("the event is sent");
    let args = h.expect_event(&alice, "net:pong");
    assert_eq!(args, vec![Value::from("pong: hi")]);
}

#[test]
fn event_not_registered_with_register_net_event_is_ignored() {
    let h = Harness::new();
    let alice = h.connect("Alice").expect("Alice is admitted");
    alice
        .send_event("net:secret", &[Value::from("x")])
        .expect("the event is sent");
    // Events from one client are handled in order, so the pong proves the
    // secret event was processed (and dropped) before this check.
    alice
        .send_event("net:ping", &[Value::from("sync")])
        .expect("the event is sent");
    h.expect_event(&alice, "net:pong");
    assert!(!h.log().contains("secret"), "log: {}", h.log());
}

#[test]
fn trigger_client_event_to_all_reaches_every_client() {
    let h = Harness::new();
    let alice = h.connect("Alice").expect("Alice is admitted");
    let bob = h.connect("Bob").expect("Bob is admitted");
    h.call("broadcast", &[Value::from("hello all")]);
    assert_eq!(
        h.expect_event(&alice, "net:all"),
        vec![Value::from("hello all")]
    );
    assert_eq!(
        h.expect_event(&bob, "net:all"),
        vec![Value::from("hello all")]
    );
}

#[test]
fn trigger_client_event_rejects_function_arguments() {
    let h = Harness::new();
    let _alice = h.connect("Alice").expect("Alice is admitted");
    let outcome = h.fx.text("net", "bad_send");
    assert!(outcome.starts_with("false "), "outcome: {outcome}");
    assert!(
        outcome.contains("functions cannot be sent"),
        "outcome: {outcome}"
    );
}

#[test]
fn drop_player_kicks_with_the_reason_and_fires_playerdropped() {
    let h = Harness::new();
    let victim = h.connect("Victim").expect("Victim is admitted");
    h.call("kick", &[Value::Int(1), Value::from("bye")]);

    let reason = h.wait_for("the kick", || match victim.recv_timeout(STEP) {
        Some(ClientEvent::Kicked { reason }) => Some(reason),
        _ => None,
    });
    assert_eq!(reason, "bye");
    h.wait_for_log("dropped:1:bye");
    assert_eq!(h.fx.text("net", "names"), "");
}

#[test]
fn client_disconnect_fires_playerdropped() {
    let h = Harness::new();
    let leaver = h.connect("Leaver").expect("Leaver is admitted");
    assert_eq!(h.fx.text("net", "names"), "1=Leaver");
    leaver.disconnect();
    h.wait_for_log("dropped:");
    assert_eq!(h.fx.text("net", "names"), "");
}

#[test]
fn getplayers_and_getplayername_see_connected_players_in_join_order() {
    let h = Harness::new();
    let _carol = h.connect("Carol").expect("Carol is admitted");
    let _dan = h.connect("Dan").expect("Dan is admitted");
    assert_eq!(h.fx.text("net", "names"), "1=Carol,2=Dan");
}

#[test]
fn set_kick_reason_after_the_dispatch_is_a_stale_reference() {
    let h = Harness::new();
    let _eve = h.connect("Eve").expect("Eve is admitted");
    let outcome = h.fx.text("net", "late");
    assert!(outcome.starts_with("false "), "outcome: {outcome}");
    assert!(outcome.contains("no longer valid"), "outcome: {outcome}");

    // The server keeps working after the stale call.
    let _frank = h.connect("Frank").expect("Frank is admitted after it");
    assert_eq!(h.fx.text("net", "names"), "1=Eve,2=Frank");
}

#[test]
fn an_invalid_endpoint_is_reported_without_starting_the_network() {
    let fx = Fixture::new();
    fx.server.execute("set endpoint_add_udp not-an-address");
    let (sender, _input) = mpsc::channel();
    let err = fx
        .server
        .start_network(sender)
        .expect_err("the endpoint is not an address");
    assert!(err.to_string().contains("not-an-address"), "error: {err}");
}
