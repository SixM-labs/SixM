//! The stock chat resource's server-side event contract over a real connection.

mod common;

use std::time::{Duration, Instant};

use common::net::{Loopback, Resource, STEP};
use sixm_core::Value;
use sixm_net::ClientEvent;

const CHAT_SERVER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/server-data/resources/[system]/chat/server.lua"
));

const PROBE_SERVER: &str = r"
local lines = {}

AddEventHandler('chatMessage', function(player, author, text)
    lines[#lines + 1] = ('%s|%s|%s'):format(player, author, text)
    if text == 'blocked' then
        CancelEvent()
    end
end)

function log()
    return table.concat(lines, '\n')
end
";

fn harness() -> Loopback {
    Loopback::new(&[
        Resource {
            name: "probe",
            script: PROBE_SERVER,
            exports: &["log"],
        },
        Resource {
            name: "chat",
            script: CHAT_SERVER,
            exports: &[],
        },
    ])
}

fn allow_chat(fx: &Loopback) {
    fx.fx
        .server
        .execute("add_ace builtin.everyone command.say allow");
}

fn assert_chat_message(args: &[Value], expected_author: &str, expected_text: &str) {
    assert_eq!(args.len(), 1, "chat:addMessage has one argument: {args:?}");
    let Value::Map(fields) = &args[0] else {
        panic!("chat:addMessage argument is not an object: {:?}", args[0]);
    };
    let Value::Array(message_args) = fields
        .iter()
        .find_map(|(key, value)| (key.as_str() == Some("args")).then_some(value))
        .expect("message has args")
    else {
        panic!("message args is not an array: {fields:?}");
    };
    assert_eq!(
        message_args,
        &[Value::from(expected_author), Value::from(expected_text),]
    );
    assert_eq!(fields.len(), 1, "default message has only args: {fields:?}");
}

#[test]
fn client_message_uses_connection_identity_and_broadcasts_fivem_shape() {
    let h = harness();
    allow_chat(&h);
    let alice = h.connect("Alice").expect("Alice is admitted");
    alice
        .send_event(
            "_chat:messageEntered",
            &[
                Value::from("hello"),
                Value::from("Impostor"),
                Value::Int(999),
            ],
        )
        .expect("message is sent");

    h.wait_for_log("1|Alice|hello");
    let message = h.expect_event(&alice, "chat:addMessage");
    assert_chat_message(&message, "Alice", "hello");
    assert!(!h.log().contains("Impostor"), "log: {}", h.log());
}

#[test]
fn chat_message_cancel_suppresses_the_default_broadcast() {
    let h = harness();
    allow_chat(&h);
    let alice = h.connect("Alice").expect("Alice is admitted");
    alice
        .send_event("_chat:messageEntered", &[Value::from("blocked")])
        .expect("message is sent");
    h.wait_for_log("1|Alice|blocked");

    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        h.pump();
        if let Some(event) = alice.recv_timeout(STEP)
            && let ClientEvent::Event { name, args } = event
        {
            assert_ne!(
                name, "chat:addMessage",
                "canceled message was broadcast: {args:?}"
            );
        }
    }

    alice
        .send_event("_chat:messageEntered", &[Value::from("accepted")])
        .expect("ordinary message is sent");
    h.wait_for_log("1|Alice|accepted");
    let message = h.expect_event(&alice, "chat:addMessage");
    assert_chat_message(&message, "Alice", "accepted");
}

#[test]
fn chat_init_returns_sorted_server_suggestions_only_to_the_requester() {
    let h = harness();
    let alice = h.connect("Alice").expect("Alice is admitted");
    let bob = h.connect("Bob").expect("Bob is admitted");

    alice.send_event("chat:init", &[]).expect("init is sent");
    let args = h.expect_event(&alice, "chat:addSuggestions");
    assert_eq!(args.len(), 1);
    let Value::Array(suggestions) = &args[0] else {
        panic!("suggestions are not an array: {:?}", args[0]);
    };
    let names: Vec<&str> = suggestions
        .iter()
        .map(|entry| {
            let Value::Map(fields) = entry else {
                panic!("suggestion is not an object: {entry:?}");
            };
            assert_eq!(fields.len(), 3, "suggestion fields: {fields:?}");
            let Value::String(name) = fields
                .iter()
                .find_map(|(key, value)| (key.as_str() == Some("name")).then_some(value))
                .expect("suggestion name")
            else {
                panic!("suggestion name is not text: {fields:?}");
            };
            std::str::from_utf8(name).expect("valid UTF-8 suggestion name")
        })
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    assert!(names.contains(&"/say"), "suggestions: {names:?}");
    assert!(names.iter().all(|name| name.starts_with('/')));

    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        h.pump();
        if let Some(ClientEvent::Event { name, args }) = bob.recv_timeout(STEP) {
            assert_ne!(name, "chat:addSuggestions", "suggestions leaked: {args:?}");
        }
    }
}

#[test]
fn server_say_uses_console_identity_and_player_submissions_need_ace() {
    let h = harness();
    let alice = h.connect("Alice").expect("Alice is admitted");

    alice
        .send_event("_chat:messageEntered", &[Value::from("denied")])
        .expect("network event is sent");
    alice
        .send_command("say denied-command")
        .expect("command is sent");
    h.fx.server.execute("say from-console");

    h.wait_for_log("0|Console|from-console");
    let message = h.expect_event(&alice, "chat:addMessage");
    assert_chat_message(&message, "Console", "from-console");
    assert!(!h.log().contains("denied"), "log: {}", h.log());

    allow_chat(&h);
    alice
        .send_event("_chat:messageEntered", &[Value::from("allowed")])
        .expect("authorized message is sent");
    h.wait_for_log("1|Alice|allowed");
    let message = h.expect_event(&alice, "chat:addMessage");
    assert_chat_message(&message, "Alice", "allowed");
}

#[test]
fn invalid_client_messages_are_dropped() {
    let h = harness();
    allow_chat(&h);
    let alice = h.connect("Alice").expect("Alice is admitted");
    for value in [
        Value::Int(7),
        Value::from("  \t  "),
        Value::from("x".repeat(257)),
    ] {
        alice
            .send_event("_chat:messageEntered", &[value])
            .expect("invalid message is sent");
    }
    alice
        .send_event("_chat:messageEntered", &[Value::from("after-invalid")])
        .expect("valid marker is sent after invalid messages");
    h.wait_for_log("1|Alice|after-invalid");
    assert_eq!(h.log(), "1|Alice|after-invalid");
    let message = h.expect_event(&alice, "chat:addMessage");
    assert_chat_message(&message, "Alice", "after-invalid");
}
