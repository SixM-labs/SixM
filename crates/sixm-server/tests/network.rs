//! Networking end to end: a server listening on loopback, real `NetClient`s and
//! a script that reacts to players.

mod common;

use std::sync::mpsc;

use common::Fixture;
use common::net::{Loopback, Resource, STEP};
use sixm_core::Value;
use sixm_net::{ClientEvent, NetError};

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

local joining_ping
AddEventHandler('playerJoining', function()
    joining_ping = GetPlayerPing(source)
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

function latent(text)
    TriggerLatentClientEvent('net:latent', -1, 25000, text)
end

function pings()
    note('pings:' .. joining_ping .. ':' .. GetPlayerPing(999))
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
fn harness() -> Loopback {
    Loopback::new(&[Resource {
        name: "net",
        script: NET_SCRIPT,
        exports: &[
            "log",
            "names",
            "broadcast",
            "latent",
            "pings",
            "kick",
            "bad_send",
            "late",
        ],
    }])
}

#[test]
fn joining_player_runs_playerjoining_and_is_listed() {
    let h = harness();
    let alice = h.connect("Alice").expect("Alice is admitted");

    assert_eq!(alice.player(), 1);
    assert_eq!(alice.server_name(), "SixM Server");
    assert!(h.log().contains("connecting:Alice"), "log: {}", h.log());
    assert!(h.log().contains("joined:1:Alice:1"), "log: {}", h.log());
    assert_eq!(h.fx.text("net", "names"), "1=Alice");
}

#[test]
fn set_kick_reason_refuses_the_connection() {
    let h = harness();
    match h.connect("reason") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "no entry for reason"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the connection should be refused"),
    }
    assert!(!h.log().contains("joined"), "log: {}", h.log());
}

#[test]
fn cancel_event_refuses_with_default_reason() {
    let h = harness();
    match h.connect("cancel") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "Rejected by the server"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the connection should be refused"),
    }
}

#[test]
fn full_server_refuses_extra_players() {
    let h = harness();
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
    let h = harness();
    let alice = h.connect("Alice").expect("Alice is admitted");
    alice
        .send_event("net:ping", &[Value::from("hi")])
        .expect("the event is sent");
    let args = h.expect_event(&alice, "net:pong");
    assert_eq!(args, vec![Value::from("pong: hi")]);
}

#[test]
fn event_not_registered_with_register_net_event_is_ignored() {
    let h = harness();
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
    let h = harness();
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
fn latent_client_event_reaches_every_client_and_ping_is_reported_in_milliseconds() {
    let h = harness();
    let alice = h.connect("Alice").expect("Alice is admitted");
    let bob = h.connect("Bob").expect("Bob is admitted");
    h.call("latent", &[Value::from("paced")]);
    assert_eq!(
        h.expect_event(&alice, "net:latent"),
        vec![Value::from("paced")]
    );
    assert_eq!(
        h.expect_event(&bob, "net:latent"),
        vec![Value::from("paced")]
    );
    h.call("pings", &[]);
    let reported = h.log();
    let pings = reported.split("pings:").nth(1).expect("ping report");
    let (player_ping, unknown_ping) = pings.split_once(':').expect("both ping values");
    assert!(player_ping.parse::<u64>().is_ok());
    assert_eq!(unknown_ping.split_whitespace().next(), Some("-1"));
}

#[test]
fn trigger_client_event_rejects_function_arguments() {
    let h = harness();
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
    let h = harness();
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
    let h = harness();
    let leaver = h.connect("Leaver").expect("Leaver is admitted");
    assert_eq!(h.fx.text("net", "names"), "1=Leaver");
    leaver.disconnect();
    h.wait_for_log("dropped:");
    assert_eq!(h.fx.text("net", "names"), "");
}

#[test]
fn getplayers_and_getplayername_see_connected_players_in_join_order() {
    let h = harness();
    let _carol = h.connect("Carol").expect("Carol is admitted");
    let _dan = h.connect("Dan").expect("Dan is admitted");
    assert_eq!(h.fx.text("net", "names"), "1=Carol,2=Dan");
}

#[test]
fn set_kick_reason_after_the_dispatch_is_a_stale_reference() {
    let h = harness();
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
