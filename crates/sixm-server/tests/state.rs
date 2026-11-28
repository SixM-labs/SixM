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
use sixm_core::{EntityInfo, EntityKind, Host, ServerHost, Value};
use sixm_net::{ClientEvent, Identity, NetClient, NetError};
use sixm_server::Input;

/// Longest a test waits for something the server should do.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long each wait step blocks before pumping the server again.
const STEP: Duration = Duration::from_millis(5);

/// The `state` resource: writes state on request and logs what its change
/// handler sees.
const STATE_SCRIPT: &str = r"
LOG = {}
REP_LOG = {}
EVENT_HITS = 0

AddStateBagChangeHandler('score', nil, function(bag, key, value)
    LOG[#LOG + 1] = bag .. '.' .. key .. '=' .. tostring(value)
end)

AddStateBagChangeHandler('replicated_probe', nil, function(bag, key, value, _, replicated)
    REP_LOG[#REP_LOG + 1] = bag .. '.' .. key .. '=' .. tostring(value) .. ':' .. tostring(replicated)
end)

RegisterNetEvent('ping:hit', function()
    EVENT_HITS = EVENT_HITS + 1
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

function rep_log()
    return table.concat(REP_LOG, ';')
end

function event_hits()
    return EVENT_HITS
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
                    "rep_log",
                    "event_hits",
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
            if let Some(ClientEvent::StateBag { bag, key, value }) = client.recv_timeout(STEP)
                && let Some(index) = wanted.iter().position(|(b, k)| *b == bag && *k == key)
            {
                found[index] = Some(value);
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
                // The join snapshot also sends the entity id lease, replicated
                // convars, the player's own ped and the resource list. None is
                // under test here.
                ClientEvent::Resources { .. }
                | ClientEvent::EntitySnapshot { .. }
                | ClientEvent::Convars { .. }
                | ClientEvent::EntityIdLease { .. } => {}
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

#[test]
fn state_keys_lists_sorted_keys_and_none_for_unknown_bags() {
    let h = Harness::new();
    h.call(
        "global_set",
        &[Value::from("b"), Value::Int(1), Value::Bool(true)],
    );
    h.call(
        "global_set",
        &[Value::from("a"), Value::Int(2), Value::Bool(false)],
    );
    let manager = h.fx.manager();
    assert_eq!(
        manager.state_keys("global"),
        Some(vec!["a".to_owned(), "b".to_owned()])
    );
    assert_eq!(manager.state_keys("player:99"), None);
    assert_eq!(manager.state_keys("entity:99"), None);
}

#[test]
fn client_state_write_updates_own_player_and_owned_entity_without_echo() {
    let h = Harness::new();
    let alice = h.connect("Alice");
    let bob = h.connect("Bob");

    let owned_entity =
        h.fx.manager()
            .create_entity(EntityInfo {
                kind: EntityKind::Vehicle,
                model: 0x1234,
                position: [0.0; 3],
                rotation: [0.0; 3],
                owner: Some(1),
            })
            .expect("entity created");
    let entity_bag = format!("entity:{owned_entity}");

    alice
        .send_state("player:1", "score", &Value::Int(42))
        .expect("own player state encodes");
    alice
        .send_state("player:1", "replicated_probe", &Value::from("ok"))
        .expect("probe state encodes");
    alice
        .send_state(&entity_bag, "fuel", &Value::Float(0.75))
        .expect("owned entity state encodes");

    assert_eq!(
        h.expect_states(
            &bob,
            &[
                ("player:1", "score"),
                ("player:1", "replicated_probe"),
                (&entity_bag, "fuel"),
            ]
        ),
        vec![Value::Int(42), Value::from("ok"), Value::Float(0.75)]
    );
    assert_eq!(
        h.fx.manager().state_get("player:1", "score"),
        Some(Value::Int(42))
    );
    assert_eq!(
        h.fx.manager().state_get(&entity_bag, "fuel"),
        Some(Value::Float(0.75))
    );
    assert_eq!(h.log(), "player:1.score=42");
    assert_eq!(
        h.value_of("rep_log", &[]),
        Value::from("player:1.replicated_probe=ok:true")
    );

    // A marker written by the server arrives on Alice without any echo of her own writes.
    h.call(
        "global_set",
        &[Value::from("marker"), Value::Int(1), Value::Bool(true)],
    );
    let seen = h.events_until_state(&alice, "global", "marker");
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            ClientEvent::StateBag { bag, .. } if bag == "player:1" || bag == &entity_bag
        )),
        "originating client must not receive an echo: {seen:?}"
    );

    // Deleting a key with Nil is also relayed to other clients and updates the server store.
    alice
        .send_state("player:1", "score", &Value::Nil)
        .expect("delete encodes");
    assert_eq!(h.expect_state(&bob, "player:1", "score"), Value::Nil);
    assert_eq!(h.fx.manager().state_get("player:1", "score"), None);
}

#[test]
fn client_state_write_rejects_unowned_bags_without_kicking() {
    let h = Harness::new();
    let alice = h.connect("Alice");
    let bob = h.connect("Bob");

    let server_entity =
        h.fx.manager()
            .create_entity(EntityInfo {
                kind: EntityKind::Object,
                model: 0x1111,
                position: [0.0; 3],
                rotation: [0.0; 3],
                owner: None,
            })
            .expect("server entity created");
    let bob_entity =
        h.fx.manager()
            .create_entity(EntityInfo {
                kind: EntityKind::Vehicle,
                model: 0x2222,
                position: [0.0; 3],
                rotation: [0.0; 3],
                owner: Some(2),
            })
            .expect("bob entity created");

    let server_bag = format!("entity:{server_entity}");
    let bob_bag = format!("entity:{bob_entity}");
    for bag in ["global", "player:2", &server_bag, &bob_bag, "entity:99999"] {
        alice
            .send_state(bag, "tampered", &Value::Int(1))
            .expect("client frame encodes");
    }
    // A valid write sent right after proves Alice stayed connected and all earlier writes arrived first.
    alice
        .send_state("player:1", "ok", &Value::Bool(true))
        .expect("valid write encodes");

    let seen = h.events_until_state(&bob, "player:1", "ok");
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, ClientEvent::StateBag { key, .. } if key == "tampered")),
        "unowned writes must not relay: {seen:?}"
    );
    for bag in ["global", "player:2", &server_bag, &bob_bag, "entity:99999"] {
        assert_eq!(h.fx.manager().state_get(bag, "tampered"), None);
    }
}

#[test]
fn client_state_strict_mode_rejects_writes_without_kicking() {
    let h = Harness::new();
    h.fx.server.execute("set sv_stateBagStrictMode true");
    let alice = h.connect("Alice");
    let bob = h.connect("Bob");

    alice
        .send_state("player:1", "blocked", &Value::Int(7))
        .expect("frame encodes");
    h.call(
        "global_set",
        &[Value::from("marker"), Value::Int(1), Value::Bool(true)],
    );
    let seen = h.events_until_state(&bob, "global", "marker");
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, ClientEvent::StateBag { key, .. } if key == "blocked")),
        "strict mode must block client writes: {seen:?}"
    );
    assert_eq!(h.fx.manager().state_get("player:1", "blocked"), None);

    // Turning strict mode off restores Alice's ability to write without reconnecting.
    h.fx.server.execute("set sv_stateBagStrictMode false");
    alice
        .send_state("player:1", "allowed", &Value::Int(8))
        .expect("frame encodes");
    assert_eq!(h.expect_state(&bob, "player:1", "allowed"), Value::Int(8));
}

#[test]
fn client_state_shares_player_event_rate_limit_and_enforces_limits() {
    let h = Harness::new();
    h.fx.server.execute("set sv_playerEventRateLimit 2"); // burst = 4
    let alice = h.connect("Alice");
    let bob = h.connect("Bob");

    // Burn 2 tokens with ordinary server events, leaving 2 tokens in the shared bucket.
    for _ in 0..2 {
        alice.send_event("ping:hit", &[]).expect("event encodes");
    }
    h.wait_for("both events to arrive", || {
        (h.value_of("event_hits", &[]) == Value::Int(2)).then_some(())
    });

    for index in 0..5 {
        alice
            .send_state("player:1", &format!("k{index}"), &Value::Int(index))
            .expect("frame encodes");
    }
    assert_eq!(h.expect_state(&bob, "player:1", "k1"), Value::Int(1));
    h.wait_for("remaining burst frames to be drained", || {
        // Pump until the connection is idle for a moment so k2..k4 are processed before refill.
        thread::sleep(Duration::from_millis(30));
        h.pump();
        Some(())
    });
    thread::sleep(Duration::from_millis(600));
    alice
        .send_state("player:1", "after", &Value::Int(99))
        .expect("post-refill frame encodes");
    assert_eq!(h.expect_state(&bob, "player:1", "after"), Value::Int(99));

    let accepted = (0..5)
        .filter(|index| {
            h.fx.manager()
                .state_get("player:1", &format!("k{index}"))
                .is_some()
        })
        .count();
    assert_eq!(
        accepted, 2,
        "2 events + 2 state writes exhaust burst size 2x sv_playerEventRateLimit"
    );

    // Client-side validation enforces name length, depth, and frame size limits.
    assert!(alice.send_state("player:1", "", &Value::Int(1)).is_err());
    let mut deep = Value::Int(1);
    for _ in 0..=64 {
        deep = Value::Array(vec![deep]);
    }
    assert!(matches!(
        alice.send_state("player:1", "deep", &deep),
        Err(NetError::TooDeep)
    ));
    let oversized = Value::String(vec![b'a'; 1024 * 1024 + 1].into_boxed_slice());
    assert!(matches!(
        alice.send_state("player:1", "big", &oversized),
        Err(NetError::FrameTooLarge { .. })
    ));
}
