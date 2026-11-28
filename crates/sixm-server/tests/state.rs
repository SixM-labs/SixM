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

mod client_runtime_tests {
    use std::net::SocketAddr;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread;
    use std::time::{Duration, Instant};

    use sixm_client::{Client, ClientRuntime};
    use sixm_core::{ClientHost, Host, Value};
    use sixm_net::Identity;
    use sixm_server::Input;
    use tempfile::TempDir;

    use crate::common::{Fixture, manifest};

    const TIMEOUT: Duration = Duration::from_secs(15);
    const STEP: Duration = Duration::from_millis(5);
    const RESOURCE: &str = "state_e2e";

    const SERVER_SCRIPT: &str = r"
local LOG = {}

AddStateBagChangeHandler(nil, nil, function(bag, key, value, _, replicated)
    LOG[#LOG + 1] = bag .. '.' .. key .. '=' .. tostring(value) .. ':' .. tostring(replicated)
end)

function server_log()
    return table.concat(LOG, ';')
end

function clear_server_log()
    LOG = {}
end

function server_global_set(key, val)
    GlobalState:set(key, val, true)
end

function server_create_entity(x, y, z, owner)
    local ent = CreateObject(`prop_box_wood01a`, x, y, z, true, false, false)
    if owner ~= nil then
        SetEntityOwner(ent, owner)
    end
    return ent
end

function server_set_owner(ent, owner)
    SetEntityOwner(ent, owner)
end

function server_move_ped(player_id, x, y, z)
    local ped = GetPlayerPed(player_id)
    SetEntityCoords(ped, x, y, z, false, false, false, false)
    return ped
end
";

    const CLIENT_LUA: &str = r"
local LOG = {}

AddStateBagChangeHandler(nil, nil, function(bag, key, value, _, replicated)
    LOG[#LOG + 1] = bag .. '.' .. key .. '=' .. tostring(value) .. ':' .. tostring(replicated)
end)

exports('lua_log', function()
    return table.concat(LOG, ';')
end)

exports('lua_clear', function()
    LOG = {}
end)

exports('lua_assign_local', function(key, val)
    LocalPlayer.state[key] = val
    return LocalPlayer.state[key]
end)

exports('lua_delete_local', function(key)
    LocalPlayer.state[key] = nil
    return LocalPlayer.state[key]
end)

exports('lua_set_local', function(key, val, replicated)
    LocalPlayer.state:set(key, val, replicated)
    return LocalPlayer.state[key]
end)

exports('lua_set_bag', function(bag, key, val, replicated)
    SetStateBagValue(bag, key, val, replicated)
    return GetStateBagValue(bag, key)
end)

exports('lua_try_global', function(key, val)
    local ok, err = pcall(function() GlobalState[key] = val end)
    return ok, tostring(err)
end)

exports('lua_try_set_bag', function(bag, key, val, replicated)
    local ok, err = pcall(SetStateBagValue, bag, key, val, replicated)
    return ok, tostring(err)
end)

exports('lua_spawn_vehicle', function(x, y, z)
    return CreateVehicle(`adder`, x, y, z, 0.0, true, false)
end)

exports('lua_entity_set', function(id, key, val, replicated)
    Entity(id).state:set(key, val, replicated)
    return Entity(id).state[key]
end)

exports('lua_try_entity_set', function(id, key, val, replicated)
    local ok, err = pcall(function() Entity(id).state:set(key, val, replicated) end)
    return ok, tostring(err)
end)

exports('lua_read_entity', function(id, key)
    return Entity(id).state[key]
end)

exports('lua_does_exist', function(id)
    return DoesEntityExist(id)
end)
";

    const CLIENT_JS: &str = r"
let jsLog = [];

AddStateBagChangeHandler(null, null, (bag, key, value, _reserved, replicated) => {
    jsLog.push(`${bag}.${key}=${String(value)}:${String(replicated)}`);
});

exports('js_log', () => jsLog.join(';'));
exports('js_clear', () => { jsLog = []; });

exports('js_assign_local', (key, val) => {
    LocalPlayer.state[key] = val;
    return LocalPlayer.state[key];
});

exports('js_set_local', (key, val, replicated) => {
    LocalPlayer.state.set(key, val, replicated);
    return LocalPlayer.state[key];
});

exports('js_delete_local', (key) => {
    delete LocalPlayer.state[key];
    return LocalPlayer.state[key];
});

exports('js_set_bag', (bag, key, val, replicated) => {
    SetStateBagValue(bag, key, val, replicated);
    return GetStateBagValue(bag, key);
});

exports('js_try_global', (key, val) => {
    try {
        GlobalState[key] = val;
        return [true, ''];
    } catch (e) {
        return [false, String(e)];
    }
});

exports('js_try_set_bag', (bag, key, val, replicated) => {
    try {
        SetStateBagValue(bag, key, val, replicated);
        return [true, ''];
    } catch (e) {
        return [false, String(e)];
    }
});

exports('js_entity_set', (id, key, val, replicated) => {
    Entity(id).state.set(key, val, replicated);
    return Entity(id).state[key];
});

exports('js_try_entity_set', (id, key, val, replicated) => {
    try {
        Entity(id).state.set(key, val, replicated);
        return [true, ''];
    } catch (e) {
        return [false, String(e)];
    }
});

exports('js_read_entity', (id, key) => Entity(id).state[key]);
";

    struct ClientHarness {
        fx: Fixture,
        input: Receiver<Input>,
        addr: SocketAddr,
    }

    struct ConnectedClient {
        runtime: Rc<ClientRuntime>,
        _dir: TempDir,
    }

    impl ClientHarness {
        fn new() -> Self {
            let fx = Fixture::new();
            let mut text = manifest(
                &["server.lua"],
                &[
                    "server_log",
                    "clear_server_log",
                    "server_global_set",
                    "server_create_entity",
                    "server_set_owner",
                    "server_move_ped",
                ],
                &[],
            );
            text.push_str("client_scripts { 'client.lua', 'client.js' }\n");
            fx.add_resource(
                RESOURCE,
                &text,
                &[
                    ("server.lua", SERVER_SCRIPT),
                    ("client.lua", CLIENT_LUA),
                    ("client.js", CLIENT_JS),
                ],
            );
            fx.server.execute("endpoint_add_udp 127.0.0.1:0");
            let (sender, input) = mpsc::channel();
            let addr = fx
                .server
                .start_network(sender)
                .expect("the network starts on loopback");
            fx.server.execute(&format!("ensure {RESOURCE}"));
            Self { fx, input, addr }
        }

        fn pump_server(&self) {
            while let Ok(input) = self.input.try_recv() {
                self.fx.server.handle(input);
            }
            self.fx.server.tick(Instant::now());
        }

        fn pump_all(&self, clients: &[&ClientRuntime]) {
            self.pump_server();
            let now = Instant::now();
            for client in clients {
                client.pump(Duration::ZERO);
                client.tick(now);
            }
        }

        fn connect(&self, name: &str) -> ConnectedClient {
            let dir = TempDir::new().expect("temp dir");
            let cache = dir.path().join("cache");
            let data = dir.path().join("data");
            let addr = self.addr;
            let name = name.to_owned();
            let handle = thread::spawn(move || {
                Client::connect(addr, &name, None, &Identity::Ephemeral, cache, TIMEOUT)
            });
            let deadline = Instant::now() + TIMEOUT;
            while !handle.is_finished() {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for client handshake"
                );
                match self.input.recv_timeout(STEP) {
                    Ok(input) => self.fx.server.handle(input),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            let client = handle
                .join()
                .expect("client thread does not panic")
                .expect("client connects and syncs");
            let runtime = ClientRuntime::new(client, data);
            runtime.start_synced();
            ConnectedClient { runtime, _dir: dir }
        }

        fn wait_until(
            &self,
            clients: &[&ClientRuntime],
            what: &str,
            mut done: impl FnMut() -> bool,
        ) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.pump_all(clients);
                if done() {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(STEP);
            }
        }

        fn server_call(&self, export: &str, args: &[Value]) -> Vec<Value> {
            self.fx
                .manager()
                .call_export(RESOURCE, export, args, None)
                .unwrap_or_else(|err| panic!("server export `{export}`: {err}"))
        }

        fn server_text(&self, export: &str) -> String {
            match self.server_call(export, &[]).into_iter().next() {
                Some(Value::String(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                other => panic!("server export `{export}` returned {other:?}"),
            }
        }
    }

    fn client_call(client: &ClientRuntime, export: &str, args: &[Value]) -> Vec<Value> {
        client
            .call_export(RESOURCE, export, args, None)
            .unwrap_or_else(|err| panic!("client export `{export}`: {err}"))
    }

    fn client_text(client: &ClientRuntime, export: &str) -> String {
        match client_call(client, export, &[]).into_iter().next() {
            Some(Value::String(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
            other => panic!("client export `{export}` returned {other:?}"),
        }
    }

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        if haystack.is_empty() {
            0
        } else {
            haystack.split(';').filter(|entry| *entry == needle).count()
        }
    }

    fn assert_single_delivery(
        alice: &ClientRuntime,
        bob: &ClientRuntime,
        server_log: &str,
        lua_entries: &[String],
        js_entries: &[String],
    ) {
        let alice_lua = client_text(alice, "lua_log");
        let alice_js = client_text(alice, "js_log");
        let bob_lua = client_text(bob, "lua_log");
        let bob_js = client_text(bob, "js_log");
        for (lua_expected, js_expected) in lua_entries.iter().zip(js_entries.iter()) {
            assert_eq!(
                count_occurrences(&alice_lua, lua_expected),
                1,
                "Alice Lua should see {lua_expected} once: {alice_lua}"
            );
            assert_eq!(
                count_occurrences(&alice_js, js_expected),
                1,
                "Alice JS should see {js_expected} once: {alice_js}"
            );
            assert_eq!(
                count_occurrences(&bob_lua, lua_expected),
                1,
                "Bob Lua should see {lua_expected} once: {bob_lua}"
            );
            assert_eq!(
                count_occurrences(&bob_js, js_expected),
                1,
                "Bob JS should see {js_expected} once: {bob_js}"
            );
            assert_eq!(
                count_occurrences(server_log, lua_expected),
                1,
                "Server should see {lua_expected} once: {server_log}"
            );
        }
    }

    fn perform_player_state_writes(alice: &ClientRuntime, prefix: &str, alice_bag: &str) {
        let immediate = client_call(
            alice,
            &format!("{prefix}_assign_local"),
            &[Value::from("stance"), Value::from("crouch")],
        );
        assert_eq!(immediate, vec![Value::from("crouch")]);
        assert_eq!(
            count_occurrences(
                &client_text(alice, &format!("{prefix}_log")),
                &format!("{alice_bag}.stance=crouch:true"),
            ),
            1
        );
        assert_eq!(
            client_call(
                alice,
                &format!("{prefix}_set_local"),
                &[Value::from("ammo"), Value::Int(30), Value::Bool(true)],
            ),
            vec![Value::Int(30)]
        );
        assert_eq!(
            client_call(
                alice,
                &format!("{prefix}_set_bag"),
                &[
                    Value::from(alice_bag),
                    Value::from("team"),
                    Value::from("blue"),
                    Value::Bool(true),
                ],
            ),
            vec![Value::from("blue")]
        );
        let deleted = client_call(
            alice,
            &format!("{prefix}_delete_local"),
            &[Value::from("stance")],
        );
        let expected_deleted = if prefix == "js" {
            Vec::new()
        } else {
            vec![Value::Nil]
        };
        assert_eq!(deleted, expected_deleted);
        assert_eq!(alice.state_get(alice_bag, "stance"), None);
        assert_eq!(
            client_call(
                alice,
                &format!("{prefix}_set_local"),
                &[
                    Value::from("hud_hint"),
                    Value::from("secret"),
                    Value::Bool(false),
                ],
            ),
            vec![Value::from("secret")]
        );
        assert_eq!(
            count_occurrences(
                &client_text(alice, &format!("{prefix}_log")),
                &format!("{alice_bag}.hud_hint=secret:false"),
            ),
            1
        );
    }

    fn verify_player_state_replication(prefix: &str, marker: &str) {
        let h = ClientHarness::new();
        let alice = h.connect("Alice");
        let bob = h.connect("Bob");
        let clients = [&*alice.runtime, &*bob.runtime];
        let alice_bag = format!("player:{}", alice.runtime.local_player());

        h.server_call("clear_server_log", &[]);
        for rt in clients {
            client_call(rt, "lua_clear", &[]);
            client_call(rt, "js_clear", &[]);
        }

        perform_player_state_writes(&alice.runtime, prefix, &alice_bag);

        h.wait_until(&clients, "server and Bob state replication", || {
            let bob_js = client_text(&bob.runtime, "js_log");
            let srv = h.server_text("server_log");
            srv.contains(&format!("{alice_bag}.team=blue:true"))
                && srv.contains(&format!("{alice_bag}.stance=nil:true"))
                && bob_js.contains(&format!("{alice_bag}.team=blue:true"))
                && bob_js.contains(&format!("{alice_bag}.stance=null:true"))
        });

        // Server marker proves Alice receives no duplicate echo of her own writes.
        h.server_call("server_global_set", &[Value::from(marker), Value::Int(1)]);
        h.wait_until(&clients, "Alice server marker", || {
            client_text(&alice.runtime, "lua_log").contains(&format!("global.{marker}=1:true"))
        });

        let lua_entries = [
            format!("{alice_bag}.stance=crouch:true"),
            format!("{alice_bag}.ammo=30:true"),
            format!("{alice_bag}.team=blue:true"),
            format!("{alice_bag}.stance=nil:true"),
        ];
        let js_entries = [
            format!("{alice_bag}.stance=crouch:true"),
            format!("{alice_bag}.ammo=30:true"),
            format!("{alice_bag}.team=blue:true"),
            format!("{alice_bag}.stance=null:true"),
        ];
        assert_single_delivery(
            &alice.runtime,
            &bob.runtime,
            &h.server_text("server_log"),
            &lua_entries,
            &js_entries,
        );

        assert_eq!(h.fx.manager().state_get(&alice_bag, "stance"), None);
        assert_eq!(
            h.fx.manager().state_get(&alice_bag, "ammo"),
            Some(Value::Int(30))
        );
        assert_eq!(
            h.fx.manager().state_get(&alice_bag, "team"),
            Some(Value::from("blue"))
        );
        assert_eq!(h.fx.manager().state_get(&alice_bag, "hud_hint"), None);
        assert_eq!(bob.runtime.state_get(&alice_bag, "hud_hint"), None);
    }

    #[test]
    fn lua_client_replicated_player_state_and_set_state_bag_value() {
        verify_player_state_replication("lua", "sync_lua");
    }

    #[test]
    fn js_client_replicated_player_state_and_set_state_bag_value() {
        verify_player_state_replication("js", "sync_js");
    }

    fn setup_owned_entities(
        h: &ClientHarness,
        alice: &ClientRuntime,
        clients: &[&ClientRuntime],
    ) -> (i64, i64) {
        let alice_id = i64::from(alice.local_player());
        let veh = client_call(
            alice,
            "lua_spawn_vehicle",
            &[Value::Float(0.0), Value::Float(0.0), Value::Float(0.0)],
        )
        .remove(0);
        let Value::Int(veh_id) = veh else {
            panic!("expected vehicle id, got {veh:?}");
        };
        assert_eq!(
            client_call(
                alice,
                "lua_entity_set",
                &[
                    Value::Int(veh_id),
                    Value::from("fuel"),
                    Value::Int(80),
                    Value::Bool(true),
                ],
            ),
            vec![Value::Int(80)]
        );

        let srv_ent = h
            .server_call(
                "server_create_entity",
                &[
                    Value::Float(0.0),
                    Value::Float(0.0),
                    Value::Float(0.0),
                    Value::Int(alice_id),
                ],
            )
            .remove(0);
        let Value::Int(srv_ent_id) = srv_ent else {
            panic!("expected server entity id, got {srv_ent:?}");
        };
        h.wait_until(clients, "Alice receives server-owned entity", || {
            client_call(alice, "lua_does_exist", &[Value::Int(srv_ent_id)])
                == vec![Value::Bool(true)]
        });
        assert_eq!(
            client_call(
                alice,
                "js_entity_set",
                &[
                    Value::Int(srv_ent_id),
                    Value::from("locked"),
                    Value::Bool(true),
                    Value::Bool(true),
                ],
            ),
            vec![Value::Bool(true)]
        );
        (veh_id, srv_ent_id)
    }

    fn client_sees_entity_states(client: &ClientRuntime, veh_id: i64, srv_ent_id: i64) -> bool {
        client_call(
            client,
            "lua_read_entity",
            &[Value::Int(veh_id), Value::from("fuel")],
        ) == vec![Value::Int(80)]
            && client_call(
                client,
                "js_read_entity",
                &[Value::Int(srv_ent_id), Value::from("locked")],
            ) == vec![Value::Bool(true)]
    }

    fn verify_non_owner_entity_writes_fail(
        h: &ClientHarness,
        alice: &ClientRuntime,
        bob: &ClientRuntime,
        clients: &[&ClientRuntime],
        veh_id: i64,
        srv_ent_id: i64,
    ) {
        assert_client_error(
            &client_call(
                bob,
                "lua_try_entity_set",
                &[
                    Value::Int(veh_id),
                    Value::from("fuel"),
                    Value::Int(0),
                    Value::Bool(true),
                ],
            ),
            "own player or owned entities",
        );
        assert_client_error(
            &client_call(
                bob,
                "js_try_entity_set",
                &[
                    Value::Int(srv_ent_id),
                    Value::from("locked"),
                    Value::Bool(false),
                    Value::Bool(true),
                ],
            ),
            "own player or owned entities",
        );
        h.server_call(
            "server_global_set",
            &[Value::from("ent_sync"), Value::Int(1)],
        );
        h.wait_until(clients, "marker after Bob disallowed writes", || {
            client_text(alice, "lua_log").contains("global.ent_sync=1:true")
        });
    }

    #[test]
    fn client_owned_entity_state_replicates_with_scoping() {
        let h = ClientHarness::new();
        h.fx.server.execute("set sixm_cullRadius 50");

        let alice = h.connect("Alice");
        let bob = h.connect("Bob");
        let charlie = h.connect("Charlie");
        let clients = [&*alice.runtime, &*bob.runtime, &*charlie.runtime];
        let bob_id = i64::from(bob.runtime.local_player());

        // Move Bob far away so he is outside the 50m cull radius around origin.
        h.server_call(
            "server_move_ped",
            &[
                Value::Int(bob_id),
                Value::Float(500.0),
                Value::Float(0.0),
                Value::Float(0.0),
            ],
        );
        for _ in 0..10 {
            h.pump_all(&clients);
            thread::sleep(STEP);
        }

        let (veh_id, srv_ent_id) = setup_owned_entities(&h, &alice.runtime, &clients);
        let veh_bag = format!("entity:{veh_id}");
        let srv_ent_bag = format!("entity:{srv_ent_id}");

        // Charlie (in scope) and server receive both keys; Bob (out of scope) does not.
        h.wait_until(&clients, "Charlie receives owned entity states", || {
            client_sees_entity_states(&charlie.runtime, veh_id, srv_ent_id)
                && h.fx.manager().state_get(&veh_bag, "fuel") == Some(Value::Int(80))
                && h.fx.manager().state_get(&srv_ent_bag, "locked") == Some(Value::Bool(true))
        });

        assert_eq!(
            client_call(&bob.runtime, "lua_does_exist", &[Value::Int(veh_id)]),
            vec![Value::Bool(false)]
        );
        assert_eq!(bob.runtime.state_get(&veh_bag, "fuel"), None);
        assert_eq!(bob.runtime.state_get(&srv_ent_bag, "locked"), None);

        // Move Bob back into scope; Bob receives the entities and their state snapshots.
        h.server_call(
            "server_move_ped",
            &[
                Value::Int(bob_id),
                Value::Float(0.0),
                Value::Float(0.0),
                Value::Float(0.0),
            ],
        );
        h.wait_until(&clients, "Bob enters scope and receives states", || {
            client_sees_entity_states(&bob.runtime, veh_id, srv_ent_id)
        });

        verify_non_owner_entity_writes_fail(
            &h,
            &alice.runtime,
            &bob.runtime,
            &clients,
            veh_id,
            srv_ent_id,
        );
        assert_eq!(
            h.fx.manager().state_get(&veh_bag, "fuel"),
            Some(Value::Int(80))
        );
        assert_eq!(
            h.fx.manager().state_get(&srv_ent_bag, "locked"),
            Some(Value::Bool(true))
        );
        verify_ownership_transfer(
            &h,
            &alice.runtime,
            &bob.runtime,
            &charlie.runtime,
            &clients,
            srv_ent_id,
            &srv_ent_bag,
        );
    }

    fn verify_ownership_transfer(
        h: &ClientHarness,
        alice: &ClientRuntime,
        bob: &ClientRuntime,
        charlie: &ClientRuntime,
        clients: &[&ClientRuntime],
        srv_ent_id: i64,
        srv_ent_bag: &str,
    ) {
        let bob_id = i64::from(bob.local_player());
        h.server_call(
            "server_set_owner",
            &[Value::Int(srv_ent_id), Value::Int(bob_id)],
        );
        h.wait_until(clients, "Alice receives ownership transfer", || {
            let res = client_call(
                alice,
                "js_try_entity_set",
                &[
                    Value::Int(srv_ent_id),
                    Value::from("locked"),
                    Value::Bool(false),
                    Value::Bool(true),
                ],
            );
            matches!(res.as_slice(), [Value::Array(items)] if items.first() == Some(&Value::Bool(false)))
        });
        assert_eq!(
            client_call(
                bob,
                "js_entity_set",
                &[
                    Value::Int(srv_ent_id),
                    Value::from("locked"),
                    Value::Bool(false),
                    Value::Bool(true),
                ],
            ),
            vec![Value::Bool(false)]
        );
        h.wait_until(
            clients,
            "transferred entity state replicates to Alice, Charlie, and server",
            || {
                client_call(
                    alice,
                    "js_read_entity",
                    &[Value::Int(srv_ent_id), Value::from("locked")],
                ) == vec![Value::Bool(false)]
                    && client_call(
                        charlie,
                        "js_read_entity",
                        &[Value::Int(srv_ent_id), Value::from("locked")],
                    ) == vec![Value::Bool(false)]
                    && h.fx.manager().state_get(srv_ent_bag, "locked") == Some(Value::Bool(false))
            },
        );
    }

    fn assert_client_error(res: &[Value], expected_substr: &str) {
        let pair = match res {
            [Value::Array(items)] => items.as_slice(),
            other => other,
        };
        assert_eq!(pair[0], Value::Bool(false), "expected failure, got {res:?}");
        let msg = match &pair[1] {
            Value::String(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("expected error text, got {other:?}"),
        };
        assert!(
            msg.contains(expected_substr),
            "expected `{expected_substr}` in `{msg}`"
        );
    }

    #[test]
    fn disallowed_client_replicated_writes_fail_on_client() {
        let h = ClientHarness::new();
        let alice = h.connect("Alice");
        let bob = h.connect("Bob");
        let clients = [&*alice.runtime, &*bob.runtime];
        let bob_bag = format!("player:{}", bob.runtime.local_player());

        // Populate a key on Bob's bag so `player:2` exists in Alice's mirror.
        client_call(
            &bob.runtime,
            "lua_assign_local",
            &[Value::from("seed"), Value::Int(1)],
        );
        h.wait_until(&clients, "Alice sees Bob bag seed", || {
            alice.runtime.state_get(&bob_bag, "seed") == Some(Value::Int(1))
        });

        let unowned_ent = h
            .server_call(
                "server_create_entity",
                &[
                    Value::Float(0.0),
                    Value::Float(0.0),
                    Value::Float(0.0),
                    Value::Nil,
                ],
            )
            .remove(0);
        let Value::Int(unowned_id) = unowned_ent else {
            panic!("expected entity id");
        };
        let unowned_bag = format!("entity:{unowned_id}");
        h.wait_until(&clients, "Alice knows unowned entity", || {
            client_call(&alice.runtime, "lua_does_exist", &[Value::Int(unowned_id)])
                == vec![Value::Bool(true)]
        });

        h.server_call("clear_server_log", &[]);
        client_call(&alice.runtime, "lua_clear", &[]);
        client_call(&alice.runtime, "js_clear", &[]);

        for prefix in ["lua", "js"] {
            assert_client_error(
                &client_call(
                    &alice.runtime,
                    &format!("{prefix}_try_global"),
                    &[Value::from("x"), Value::Int(1)],
                ),
                "own player or owned entities",
            );
            assert_client_error(
                &client_call(
                    &alice.runtime,
                    &format!("{prefix}_try_set_bag"),
                    &[
                        Value::from(bob_bag.as_str()),
                        Value::from("x"),
                        Value::Int(1),
                        Value::Bool(true),
                    ],
                ),
                "own player or owned entities",
            );
            assert_client_error(
                &client_call(
                    &alice.runtime,
                    &format!("{prefix}_try_entity_set"),
                    &[
                        Value::Int(unowned_id),
                        Value::from("x"),
                        Value::Int(1),
                        Value::Bool(true),
                    ],
                ),
                "own player or owned entities",
            );
            assert_client_error(
                &client_call(
                    &alice.runtime,
                    &format!("{prefix}_try_set_bag"),
                    &[
                        Value::from("entity:99999"),
                        Value::from("x"),
                        Value::Int(1),
                        Value::Bool(true),
                    ],
                ),
                "does not exist",
            );
        }

        h.server_call("server_global_set", &[Value::from("ok"), Value::Int(1)]);
        h.wait_until(&clients, "marker after disallowed writes", || {
            client_text(&bob.runtime, "lua_log").contains("global.ok=1:true")
        });

        assert_eq!(client_text(&alice.runtime, "lua_log"), "global.ok=1:true");
        assert_eq!(client_text(&alice.runtime, "js_log"), "global.ok=1:true");
        for bag in ["global", bob_bag.as_str(), unowned_bag.as_str()] {
            assert_eq!(h.fx.manager().state_get(bag, "x"), None);
            assert_eq!(bob.runtime.state_get(bag, "x"), None);
        }
    }

    #[test]
    fn strict_mode_ignores_client_script_replicated_writes() {
        let h = ClientHarness::new();
        h.fx.server.execute("set sv_stateBagStrictMode true");

        let alice = h.connect("Alice");
        let bob = h.connect("Bob");
        let clients = [&*alice.runtime, &*bob.runtime];
        let alice_bag = format!("player:{}", alice.runtime.local_player());

        client_call(
            &alice.runtime,
            "lua_assign_local",
            &[Value::from("blocked_lua"), Value::Int(1)],
        );
        client_call(
            &alice.runtime,
            "js_assign_local",
            &[Value::from("blocked_js"), Value::Int(2)],
        );

        h.server_call(
            "server_global_set",
            &[Value::from("strict_marker"), Value::Bool(true)],
        );
        h.wait_until(&clients, "Bob receives strict mode marker", || {
            client_text(&bob.runtime, "lua_log").contains("global.strict_marker=true:true")
        });

        assert_eq!(h.fx.manager().state_get(&alice_bag, "blocked_lua"), None);
        assert_eq!(h.fx.manager().state_get(&alice_bag, "blocked_js"), None);
        assert_eq!(bob.runtime.state_get(&alice_bag, "blocked_lua"), None);
        assert_eq!(bob.runtime.state_get(&alice_bag, "blocked_js"), None);

        // Turning strict mode off lets subsequent writes replicate normally.
        h.fx.server.execute("set sv_stateBagStrictMode false");
        client_call(
            &alice.runtime,
            "js_set_local",
            &[
                Value::from("after_strict"),
                Value::from("ok"),
                Value::Bool(true),
            ],
        );
        h.wait_until(
            &clients,
            "Bob receives write after strict mode disabled",
            || {
                bob.runtime.state_get(&alice_bag, "after_strict") == Some(Value::from("ok"))
                    && h.fx.manager().state_get(&alice_bag, "after_strict")
                        == Some(Value::from("ok"))
            },
        );
    }
}
