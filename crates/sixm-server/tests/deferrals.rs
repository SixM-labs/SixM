//! Connection deferrals end to end: `playerConnecting` handlers that hold a
//! connection with `defer`, report with `update` and finish with `done`.

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::net::{Loopback, Resource, TIMEOUT};
use sixm_net::NetError;

/// The `defer` resource. Each connecting player name selects a behaviour.
const DEFER_SCRIPT: &str = r#"
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
    elseif name == 'CardLua' then
        deferrals.defer()
        deferrals.presentCard({
            type = 'AdaptiveCard',
            version = '1.5',
            body = {
                { type = 'TextBlock', text = 'Enter access code' },
            },
        }, function(data, raw)
            note('card_lua:' .. tostring(data and data.code) .. ':' .. tostring(data and data.agree) .. ':' .. tostring(raw))
            deferrals.done()
        end)
    elseif name == 'CardCancel' then
        deferrals.defer()
        deferrals.presentCard('{"type":"AdaptiveCard","title":"Cancel Test"}', function(data, raw)
            note('card_cancel:' .. tostring(data == nil) .. ':' .. tostring(raw == nil))
            deferrals.done('Canceled')
        end)
    elseif name == 'CardHeld' then
        deferrals.defer()
        HELD = deferrals
        HELD_ID = source
        deferrals.presentCard('{"type":"AdaptiveCard","title":"Held Card"}', function(data, raw)
            note('card_held:' .. tostring(data) .. ':' .. tostring(raw))
            deferrals.done()
        end)
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
"#;

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

#[test]
fn present_card_delivers_lua_table_and_admits_on_response() {
    let h = harness();
    let (card_tx, cards) = mpsc::channel();
    let player = h
        .connect_with_cards(
            "CardLua",
            |_| {},
            move |card_json| {
                let _ = card_tx.send(card_json.to_owned());
                Some(r#"{"code":"open","agree":true}"#.to_owned())
            },
        )
        .expect("CardLua is admitted");

    let received: Vec<String> = cards.try_iter().collect();
    assert_eq!(received.len(), 1);
    let parsed: serde_json::Value =
        serde_json::from_str(&received[0]).expect("card payload is valid JSON");
    assert_eq!(parsed["type"], "AdaptiveCard");
    assert_eq!(parsed["body"][0]["text"], "Enter access code");

    assert_eq!(player.player(), 1);
    h.wait_for_log("joined:1:CardLua");
    assert!(
        h.log()
            .contains(r#"card_lua:open:true:{"code":"open","agree":true}"#),
        "log: {}",
        h.log()
    );
    assert_eq!(h.fx.text("defer", "names"), "1=CardLua");
}

#[test]
fn present_card_in_js_supports_json_string_and_chained_cards() {
    let h = harness();
    let js_script = r"
        const LOG = [];
        on('playerConnecting', (name, _setKickReason, deferrals) => {
            if (name !== 'CardJs') return;
            deferrals.defer();
            deferrals.presentCard(
                JSON.stringify({ type: 'AdaptiveCard', step: 1 }),
                (data, rawData) => {
                    LOG.push(`step1:${data?.choice}:${rawData}`);
                    deferrals.presentCard(
                        { type: 'AdaptiveCard', step: 2 },
                        (data2, rawData2) => {
                            LOG.push(`step2:${data2?.confirm}:${rawData2}`);
                            deferrals.done();
                        },
                    );
                },
            );
        });
        on('playerJoining', () => {
            if (GetPlayerName(source) === 'CardJs') {
                LOG.push(`joined:${source}`);
            }
        });
        exports('log', () => LOG.join(' '));
    ";
    h.fx.add_resource(
        "js_cards",
        &common::manifest(&["server.js"], &["log"], &[]),
        &[("server.js", js_script)],
    );
    h.fx.server.execute("refresh");
    h.fx.server.execute("ensure js_cards");

    let (card_tx, cards) = mpsc::channel();
    let mut step = 0usize;
    let player = h
        .connect_with_cards(
            "CardJs",
            |_| {},
            move |card_json| {
                let _ = card_tx.send(card_json.to_owned());
                step += 1;
                if step == 1 {
                    Some(r#"{"choice":"alpha"}"#.to_owned())
                } else {
                    Some(r#"{"confirm":true}"#.to_owned())
                }
            },
        )
        .expect("CardJs is admitted after two cards");

    let received: Vec<String> = cards.try_iter().collect();
    assert_eq!(received.len(), 2);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&received[0]).unwrap()["step"],
        1
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&received[1]).unwrap()["step"],
        2
    );

    assert_eq!(player.player(), 1);
    h.wait_for("JS card log", || {
        let log = h.fx.text("js_cards", "log");
        log.contains("joined:1").then_some(log)
    });
    let js_log = h.fx.text("js_cards", "log");
    assert!(
        js_log.contains(r#"step1:alpha:{"choice":"alpha"}"#),
        "js_log: {js_log}"
    );
    assert!(
        js_log.contains(r#"step2:true:{"confirm":true}"#),
        "js_log: {js_log}"
    );
}

#[test]
fn present_card_cancel_passes_nil_to_callback_and_refuses() {
    let h = harness();
    match h.connect_with_cards("CardCancel", |_| {}, |_| None) {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "Canceled"),
        Err(other) => panic!("expected refusal, got {other}"),
        Ok(_) => panic!("canceled card should be refused"),
    }
    h.wait_for_log("card_cancel:true:true");
    assert!(!h.log().contains("joined"), "log: {}", h.log());
}

#[test]
fn expired_card_waiter_cleans_up_pending_player_and_callbacks() {
    let h = harness();
    h.fx.server.execute("set sixm_deferralTimeout 1");

    let (shown_tx, shown_rx) = mpsc::channel();
    let (unblock_tx, unblock_rx) = mpsc::channel::<()>();
    let handshake = h.spawn_connect_with_cards(
        "CardHeld",
        |_| {},
        move |_| {
            let _ = shown_tx.send(());
            let _ = unblock_rx.recv_timeout(Duration::from_secs(5));
            None
        },
        TIMEOUT,
    );

    h.wait_for("card to be delivered to client", || {
        shown_rx.try_recv().ok()
    });
    assert_eq!(h.fx.text("defer", "held_name"), "CardHeld");

    h.wait_for("pending card deferral to expire", || {
        (h.fx.text("defer", "held_name") == "none").then_some(())
    });
    let outcome = h.fx.text("defer", "release");
    assert!(outcome.starts_with("false "), "outcome: {outcome}");
    assert!(outcome.contains("no longer valid"), "outcome: {outcome}");
    assert!(!h.log().contains("card_held:"), "log: {}", h.log());
    assert!(!h.log().contains("joined"), "log: {}", h.log());

    let _ = unblock_tx.send(());
    match h.finish_connect(handshake) {
        Err(NetError::Rejected(reason)) => assert_eq!(reason, "connection timed out"),
        Err(other) => panic!("expected deferral timeout refusal, got {other}"),
        Ok(_) => panic!("timed out card should be refused"),
    }
}
