//! Connection deferrals end to end: `playerConnecting` handlers that hold a
//! connection with `defer`, report with `update` and finish with `done`.

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::net::{Loopback, Resource, TIMEOUT};
use sixm_net::NetError;

/// The `defer` resource. Each connecting player name selects a behaviour.
const DEFER_SCRIPT: &str = r"
LOG = {}
HELD = nil
HELD_ID = nil

local function note(entry)
    LOG[#LOG + 1] = entry
end

AddEventHandler('playerConnecting', function(name, setKickReason, deferrals)
    note('connecting:' .. name)
    if name == 'Waiter' then
        deferrals.defer()
        CreateThread(function()
            Wait(0)
            Wait(0)
            deferrals.update('step one')
            deferrals.update('step two')
            deferrals.done()
        end)
    elseif name == 'Banned' then
        deferrals.defer()
        CreateThread(function()
            Wait(0)
            deferrals.done('Banned')
        end)
    elseif name == 'Stale' then
        deferrals.defer()
        CreateThread(function()
            Wait(0)
            deferrals.done()
            Wait(0)
            local ok, err = pcall(deferrals.update, 'late')
            note('late:' .. tostring(ok) .. ':' .. tostring(err))
        end)
    elseif name == 'Held' then
        deferrals.defer()
        HELD = deferrals
        HELD_ID = source
    end
end)

AddEventHandler('playerJoining', function()
    note('joined:' .. source .. ':' .. GetPlayerName(source))
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

function held_name()
    return GetPlayerName(HELD_ID) or 'none'
end

function held_count()
    return tostring(#GetPlayers())
end

function release()
    local ok, err = pcall(HELD.done)
    return tostring(ok) .. ' ' .. tostring(err)
end
";

/// The first of two resources that both defer the player `Both`.
const FIRST_SCRIPT: &str = r"
LOG = {}

AddEventHandler('playerConnecting', function(name, setKickReason, deferrals)
    if name == 'Both' then
        deferrals.defer()
        CreateThread(function()
            Wait(0)
            LOG[#LOG + 1] = 'first:done'
            deferrals.done()
        end)
    end
end)

function log()
    return table.concat(LOG, ' ')
end
";

/// The second resource. It finishes only when `release` is called.
const SECOND_SCRIPT: &str = r"
LOG = {}
HELD = nil

AddEventHandler('playerConnecting', function(name, setKickReason, deferrals)
    if name == 'Both' then
        deferrals.defer()
        HELD = deferrals
    end
end)

AddEventHandler('playerJoining', function()
    LOG[#LOG + 1] = 'joined'
end)

function log()
    return table.concat(LOG, ' ')
end

function release()
    HELD.done()
end
";

/// A server with the `defer` resource running.
fn harness() -> Loopback {
    Loopback::new(&[Resource {
        name: "defer",
        script: DEFER_SCRIPT,
        exports: &["log", "names", "held_name", "held_count", "release"],
    }])
}

#[test]
fn updates_reach_the_client_and_done_admits_the_player() {
    let h = harness();
    let (sender, updates) = mpsc::channel();
    let alice = h
        .connect_observed("Waiter", move |message| {
            let _ = sender.send(message.to_owned());
        })
        .expect("Waiter is admitted");

    let received: Vec<String> = updates.try_iter().collect();
    assert_eq!(received, vec!["step one", "step two"]);
    assert_eq!(alice.player(), 1);
    h.wait_for_log("joined:1:Waiter");
    assert_eq!(h.fx.text("defer", "names"), "1=Waiter");
}

#[test]
fn done_with_a_reason_refuses_the_player() {
    let h = harness();
    match h.connect("Banned") {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "Banned"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the connection should be refused"),
    }
    assert!(!h.log().contains("joined"), "log: {}", h.log());
}

#[test]
fn every_deferring_resource_must_finish_before_the_player_joins() {
    let h = Loopback::new(&[
        Resource {
            name: "first",
            script: FIRST_SCRIPT,
            exports: &["log"],
        },
        Resource {
            name: "second",
            script: SECOND_SCRIPT,
            exports: &["log", "release"],
        },
    ]);
    let handshake = h.spawn_connect("Both", |_| {}, TIMEOUT);
    h.wait_for("the first resource to finish", || {
        h.fx.text("first", "log")
            .contains("first:done")
            .then_some(())
    });

    // The first resource is done, but the second still holds the player.
    assert!(!handshake.is_finished(), "the player joined too early");
    assert!(!h.fx.text("second", "log").contains("joined"));

    h.call_in("second", "release", &[]);
    let alice = h
        .finish_connect(handshake)
        .expect("the player joins once both resources are done");
    assert_eq!(alice.player(), 1);
    assert!(h.fx.text("second", "log").contains("joined"));
}

#[test]
fn a_pending_player_is_known_but_not_listed() {
    let h = harness();
    let handshake = h.spawn_connect("Held", |_| {}, TIMEOUT);
    h.wait_for_log("connecting:Held");

    assert_eq!(h.fx.text("defer", "held_name"), "Held");
    assert_eq!(h.fx.text("defer", "held_count"), "0");
    assert_eq!(h.fx.text("defer", "names"), "");
    assert!(!handshake.is_finished());

    h.call("release", &[]);
    let held = h
        .finish_connect(handshake)
        .expect("Held joins after release");
    assert_eq!(held.player(), 1);
    assert_eq!(h.fx.text("defer", "names"), "1=Held");
}

#[test]
fn a_pending_player_does_not_count_against_the_player_limit() {
    let h = harness();
    h.fx.server.execute("set sv_maxclients 1");
    let handshake = h.spawn_connect("Held", |_| {}, TIMEOUT);
    h.wait_for_log("connecting:Held");

    let other = h
        .connect("Other")
        .expect("the pending player takes no slot");
    assert_eq!(other.player(), 2);

    // Once the limit is reached, the player that was held is refused as full.
    h.call("release", &[]);
    match h.finish_connect(handshake) {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "Server is full"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the held player should be refused"),
    }
}

#[test]
fn a_deferral_not_finished_in_time_is_refused() {
    let h = harness();
    h.fx.server.execute("set sixm_deferralTimeout 1");
    let started = Instant::now();
    let handshake = h.spawn_connect("Held", |_| {}, TIMEOUT);
    match h.finish_connect(handshake) {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "connection timed out"),
        Err(other) => panic!("expected a refusal, got {other}"),
        Ok(_) => panic!("the deferral should time out"),
    }
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(!h.log().contains("joined"), "log: {}", h.log());
}

#[test]
fn a_player_that_disconnects_while_deferred_is_forgotten_quietly() {
    let h = harness();
    let handshake = h.spawn_connect("Held", |_| {}, Duration::from_millis(400));
    h.wait_for_log("connecting:Held");

    // The client gives up after its own deadline and closes the connection.
    assert!(matches!(
        h.finish_connect(handshake),
        Err(NetError::Timeout)
    ));
    h.wait_for("the abandoned player to be forgotten", || {
        (h.fx.text("defer", "held_name") == "none").then_some(())
    });

    assert!(!h.log().contains("joined"), "log: {}", h.log());
    assert!(!h.log().contains("dropped"), "log: {}", h.log());
    assert_eq!(h.fx.text("defer", "names"), "");
    let outcome = h.fx.text("defer", "release");
    assert!(outcome.starts_with("false "), "outcome: {outcome}");
    assert!(outcome.contains("no longer valid"), "outcome: {outcome}");
}

#[test]
fn update_after_done_is_a_stale_reference_that_pcall_catches() {
    let h = harness();
    let _stale = h.connect("Stale").expect("Stale is admitted");
    h.wait_for_log("late:false");
    assert!(h.log().contains("no longer valid"), "log: {}", h.log());
    assert!(h.log().contains("joined:1:Stale"), "log: {}", h.log());
}
