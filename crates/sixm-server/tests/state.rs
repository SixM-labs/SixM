//! State bags end to end: a server on loopback, real `NetClient`s and a script
//! that writes state.
//!
//! As in `network.rs`, the test thread plays the server loop, because the
//! client calls block.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_core::{Host, Value};
use sixm_net::{ClientEvent, Identity, NetClient};
use sixm_server::Input;

/// Longest a test waits for something the server should do.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long each wait step blocks before pumping the server again.
const STEP: Duration = Duration::from_millis(5);

/// The `state` resource: writes state on request and logs what its change
/// handler sees.
const STATE_SCRIPT: &str = r"
LOG = {}

AddStateBagChangeHandler('score', nil, function(bag, key, value)
    LOG[#LOG + 1] = bag .. '.' .. key .. '=' .. tostring(value)
end)

function global_set(key, value, replicated)
    GlobalState:set(key, value, replicated)
end

function global_fill(count)
    for i = 1, count do
        GlobalState:set('key' .. i, i, true)
    end
end

function player_set(id, key, value)
    Player(id).state[key] = value
end

function try_player(id)
    local ok, err = pcall(function() Player(id).state.x = 1 end)
    return tostring(ok) .. ' ' .. tostring(err)
end

function read_global(key)
    return GlobalState[key]
end

function log()
    return table.concat(LOG, ';')
end
";

/// A server on loopback with the `state` resource running.
struct Harness {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
}

impl Harness {
    fn new() -> Self {
        let fx = Fixture::new();
        fx.add_resource(
            "state",
            &manifest(
                &["server.lua"],
                &[
                    "global_set",
                    "global_fill",
                    "player_set",
                    "try_player",
                    "read_global",
                    "log",
                ],
                &[],
            ),
            &[("server.lua", STATE_SCRIPT)],
        );
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        fx.server.execute("ensure state");
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
    fn connect(&self, name: &str) -> NetClient {
        let addr = self.addr;
        let name = name.to_owned();
        let handshake =
            thread::spawn(move || NetClient::connect(addr, &name, &Identity::Ephemeral, TIMEOUT));
        loop {
            self.pump();
            if handshake.is_finished() {
                return handshake
                    .join()
                    .expect("the client thread does not panic")
                    .expect("the client is admitted");
            }
            thread::sleep(STEP);
        }
    }

    /// Calls `export` of `state` with `args`, discarding its results.
    fn call(&self, export: &str, args: &[Value]) {
        self.value_of(export, args);
    }

    /// Calls `export` of `state` with `args` and returns its first result.
    fn value_of(&self, export: &str, args: &[Value]) -> Value {
        self.fx
            .manager()
            .call_export("state", export, args, None)
            .unwrap_or_else(|err| panic!("export `{export}`: {err}"))
            .into_iter()
            .next()
            .unwrap_or(Value::Nil)
    }

    /// Waits until `client` has received every `(bag, key)` in `wanted`, and
    /// returns their values in the same order. Other events are skipped.
    fn expect_states(&self, client: &NetClient, wanted: &[(&str, &str)]) -> Vec<Value> {
        let mut found: Vec<Option<Value>> = vec![None; wanted.len()];
        self.wait_for("the expected state changes", || {
            if let Some(ClientEvent::StateBag { bag, key, value }) = client.recv_timeout(STEP) {
                if let Some(index) = wanted.iter().position(|(b, k)| *b == bag && *k == key) {
                    found[index] = Some(value);
                }
            }
            found.iter().all(Option::is_some).then_some(())
        });
        found.into_iter().flatten().collect()
    }

    /// Waits for the state change of `bag.key` on `client` and returns its value.
    fn expect_state(&self, client: &NetClient, bag: &str, key: &str) -> Value {
        self.expect_states(client, &[(bag, key)]).remove(0)
    }

    /// Receives every event on `client` up to the state change of `bag.key`,
    /// and returns them all, including that change.
    fn events_until_state(&self, client: &NetClient, bag: &str, key: &str) -> Vec<ClientEvent> {
        let mut seen = Vec::new();
        self.wait_for(&format!("state {bag}.{key}"), || {
            let event = client.recv_timeout(STEP)?;
            let done = matches!(
                &event,
                ClientEvent::StateBag { bag: received_bag, key: received_key, .. }
                    if received_bag == bag && received_key == key
            );
            seen.push(event);
            done.then_some(())
        });
        seen
    }

    /// The `state` resource's log as one string.
    fn log(&self) -> String {
        match self.value_of("log", &[]) {
            Value::String(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            other => panic!("log returned {other:?}"),
        }
    }
}

#[test]
fn joining_client_receives_the_replicated_snapshot() {
    let h = Harness::new();
    h.call(
        "global_set",
        &[
            Value::from("greeting"),
            Value::from("hi"),
            Value::Bool(true),
        ],
    );
    let alice = h.connect("Alice");
    assert_eq!(
        h.expect_states(&alice, &[("global", "greeting")]),
        vec![Value::from("hi")]
    );
    h.call(
        "player_set",
        &[Value::Int(1), Value::from("color"), Value::from("red")],
    );
    assert_eq!(
        h.expect_states(&alice, &[("player:1", "color")]),
        vec![Value::from("red")]
    );

    let bob = h.connect("Bob");
    assert_eq!(
        h.expect_states(&bob, &[("global", "greeting"), ("player:1", "color")]),
        vec![Value::from("hi"), Value::from("red")],
        "the snapshot holds the global bag and every player bag"
    );
}

#[test]
fn joining_client_with_thousands_of_replicated_keys_is_not_kicked() {
    const KEYS: i64 = 3000;
    let h = Harness::new();
    h.call("global_fill", &[Value::Int(KEYS)]);

    let alice = h.connect("Alice");
    let mut received = HashMap::new();
    h.wait_for("the whole snapshot", || {
        while let Some(event) = alice.recv_timeout(Duration::ZERO) {
            match event {
                // The resource list comes first on join, and the player's own ped
                // is in the entity snapshot. Neither is under test here.
                ClientEvent::Resources { .. } | ClientEvent::EntitySnapshot { .. } => {}
                ClientEvent::StateBag { bag, key, value } => {
                    assert_eq!(bag, "global");
                    received.insert(key, value);
                }
                other => panic!("unexpected event while the snapshot arrives: {other:?}"),
            }
        }
        (received.len() == usize::try_from(KEYS).expect("a small count")).then_some(())
    });
    for i in 1..=KEYS {
        assert_eq!(received.get(&format!("key{i}")), Some(&Value::Int(i)));
    }

    // A kick would arrive after the snapshot, so keep pumping for a moment.
    for _ in 0..20 {
        h.pump();
        assert!(
            alice.recv_timeout(STEP).is_none(),
            "the client receives nothing after the snapshot"
        );
    }
}

#[test]
fn replicated_write_reaches_every_client() {
    let h = Harness::new();
    let alice = h.connect("Alice");
    let bob = h.connect("Bob");
    h.call(
        "global_set",
        &[Value::from("mode"), Value::from("race"), Value::Bool(true)],
    );
    assert_eq!(
        h.expect_state(&alice, "global", "mode"),
        Value::from("race")
    );
    assert_eq!(h.expect_state(&bob, "global", "mode"), Value::from("race"));

    h.call(
        "global_set",
        &[Value::from("mode"), Value::Nil, Value::Bool(true)],
    );
    assert_eq!(
        h.expect_state(&alice, "global", "mode"),
        Value::Nil,
        "a deleted key is sent as nil"
    );
}

#[test]
fn non_replicated_write_stays_on_the_server() {
    let h = Harness::new();
    let alice = h.connect("Alice");
    h.call(
        "global_set",
        &[Value::from("hidden"), Value::Int(1), Value::Bool(false)],
    );
    h.call(
        "global_set",
        &[Value::from("marker"), Value::Int(2), Value::Bool(true)],
    );

    let seen = h.events_until_state(&alice, "global", "marker");
    let leaked = seen
        .iter()
        .any(|event| matches!(event, ClientEvent::StateBag { key, .. } if key == "hidden"));
    assert!(!leaked, "events: {seen:?}");
    assert_eq!(
        h.value_of("read_global", &[Value::from("hidden")]),
        Value::Int(1),
        "the server still holds the value"
    );
}

#[test]
fn player_bag_write_reaches_every_client() {
    let h = Harness::new();
    let alice = h.connect("Alice");
    let bob = h.connect("Bob");
    h.call(
        "player_set",
        &[Value::Int(1), Value::from("ready"), Value::Bool(true)],
    );
    assert_eq!(
        h.expect_state(&alice, "player:1", "ready"),
        Value::Bool(true)
    );
    assert_eq!(h.expect_state(&bob, "player:1", "ready"), Value::Bool(true));
}

#[test]
fn dropping_a_player_removes_its_bag_and_later_writes_fail() {
    let h = Harness::new();
    let alice = h.connect("Alice");
    let bob = h.connect("Bob");
    h.call(
        "player_set",
        &[Value::Int(1), Value::from("ready"), Value::Bool(true)],
    );
    h.expect_state(&bob, "player:1", "ready");

    alice.disconnect();
    // The player's ped is removed with its own state bag, which comes first.
    let removed = h.wait_for("the removal of player:1", || match bob.recv_timeout(STEP) {
        Some(ClientEvent::StateBagRemoved { bag }) if bag == "player:1" => Some(bag),
        _ => None,
    });
    assert_eq!(removed, "player:1");

    let outcome = h.value_of("try_player", &[Value::Int(1)]);
    let Value::String(bytes) = outcome else {
        panic!("try_player returned {outcome:?}");
    };
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.starts_with("false "), "outcome: {text}");
    assert!(text.contains("does not exist"), "outcome: {text}");
}

#[test]
fn change_handlers_run_on_the_server_for_every_write() {
    let h = Harness::new();
    let _alice = h.connect("Alice");
    h.call(
        "global_set",
        &[Value::from("score"), Value::Int(7), Value::Bool(false)],
    );
    h.call(
        "global_set",
        &[Value::from("score"), Value::Int(8), Value::Bool(true)],
    );
    assert_eq!(h.log(), "global.score=7;global.score=8");
}

#[test]
fn state_console_command_lists_and_prints_bags() {
    let h = Harness::new();
    let _alice = h.connect("Alice");
    h.call(
        "player_set",
        &[Value::Int(1), Value::from("ready"), Value::Bool(true)],
    );
    h.fx.server.execute("state");
    h.fx.server.execute("state global");
    h.fx.server.execute("state player:1");
    h.fx.server.execute("state player:9");
}
