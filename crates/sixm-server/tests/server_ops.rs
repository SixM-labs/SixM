//! Integration tests for server operations: Chrome Trace `profiler` commands
//! and authenticated `POST /rcon` (`rcon_password`).

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::net::{Loopback, Resource, STEP};
use common::{Fixture, manifest, write_file};
use serde_json::Value as Json;
use sixm_core::{Host, Value};
use sixm_net::ClientEvent;

const RCON_TEST_RESOURCE: Resource<'static> = Resource {
    name: "app",
    script: r"
        local changes = 0
        AddConvarChangeListener(nil, function(name)
            if name:lower() == 'rcon_password' then changes = changes + 1 end
        end)
        RegisterCommand('rcon_ping', function()
            print('pong from rcon')
            ExecuteCommand('help')
        end, false)
        exports('credentials', function()
            local changed = pcall(SetConvar, 'rcon_password', 'script-password')
            ExecuteCommand('set rcon_password queued-password')
            return GetConvar('RCON_PASSWORD', 'hidden'), changes, changed
        end)
    ",
    exports: &[],
};

struct RemoveOnDrop(String);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn post_raw(addr: SocketAddr, extra_headers: &str, body: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).expect("tcp connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let req = format!(
        "POST /rcon HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write request");
    let mut data = Vec::new();
    stream.read_to_end(&mut data).expect("read response");
    let split = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head")
        + 4;
    let head = String::from_utf8_lossy(&data[..split]);
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("u16 status");
    (status, data[split..].to_vec())
}

fn send_rcon(lb: &Loopback, extra_headers: &str, body: &str) -> (u16, Vec<u8>) {
    let (tx, rx) = mpsc::channel();
    let addr = lb.addr;
    let extra = extra_headers.to_owned();
    let payload = body.to_owned();
    thread::spawn(move || {
        let _ = tx.send(post_raw(addr, &extra, &payload));
    });
    lb.wait_for("rcon HTTP response", || rx.try_recv().ok())
}

#[test]
fn profiler_records_resources_and_saves_chrome_trace_json() {
    let fx = Fixture::new();
    fx.add_resource(
        "prof_res",
        &manifest(&["server.lua"], &[], &[]),
        &[(
            "server.lua",
            "CreateThread(function() while true do Wait(0) end end)\n",
        )],
    );
    fx.server.execute("ensure prof_res");

    fx.server.execute("profiler record 3");
    for _ in 0..4 {
        fx.server.tick(Instant::now());
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let rel = format!("target/sixm_test_profile_{nanos}.json");
    let _cleanup = RemoveOnDrop(rel.clone());

    fx.server.execute("profiler saveJSON ../escape.json");
    fx.server.execute(&format!("profiler saveJSON {rel}"));

    let bytes = fs::read(&rel).expect("saved profiler json");
    let doc: Json = serde_json::from_slice(&bytes).expect("valid trace json");
    let events = doc["traceEvents"].as_array().expect("traceEvents array");
    assert!(
        events
            .iter()
            .any(|e| e["name"] == "prof_res" && e["cat"] == "resource" && e["ph"] == "X"),
        "expected resource span in {doc}"
    );
    let ticks = events
        .iter()
        .filter(|e| e["name"] == "ServerTick" && e["cat"] == "frame")
        .count();
    assert_eq!(ticks, 3, "expected auto-stop after 3 frames: {doc}");
}

#[test]
fn rcon_enforces_password_rate_limit_and_returns_command_output() {
    let lb = Loopback::new(&[RCON_TEST_RESOURCE]);

    // Disabled by default when rcon_password is unset/empty.
    let (status, _) = send_rcon(&lb, "", r#"{"password":"secret","command":"help"}"#);
    assert_eq!(status, 403);

    lb.fx.server.execute("set rcon_password super-secret-pw");

    // 5 wrong attempts trigger rate lockout (`429`) on the 6th.
    for _ in 0..5 {
        let (st, _) = send_rcon(&lb, "", r#"{"password":"wrong","command":"help"}"#);
        assert_eq!(st, 401);
    }
    let (locked, _) = send_rcon(
        &lb,
        "",
        r#"{"password":"super-secret-pw","command":"help"}"#,
    );
    assert_eq!(locked, 429);

    // Re-test success on a fresh server so the test finishes in milliseconds.
    let lb2 = Loopback::new(&[RCON_TEST_RESOURCE]);
    lb2.fx
        .server
        .execute("add_ace resource.app command.set allow");
    lb2.fx
        .server
        .execute("add_ace resource.app command.help allow");
    lb2.fx.server.execute("set RCON_PASSWORD super-secret-pw");
    let credentials = lb2
        .fx
        .manager()
        .call_export("app", "credentials", &[], None)
        .expect("read credentials export");
    assert_eq!(
        credentials,
        [Value::from("hidden"), Value::Int(0), Value::Bool(false)]
    );
    lb2.pump();
    lb2.fx.server.execute("sets rcon_password public-password");
    lb2.fx
        .server
        .execute("setr rcon_password replicated-password");

    let (ok_status, body) = send_rcon(
        &lb2,
        "Authorization: Bearer super-secret-pw\r\n",
        r#"{"command":"rcon_ping"}"#,
    );
    assert_eq!(ok_status, 200);
    let doc: Json = serde_json::from_slice(&body).expect("rcon json");
    assert_eq!(doc["ok"], true);
    let out = doc["output"].as_array().expect("output array");
    assert!(
        out.iter()
            .any(|line| line.as_str().is_some_and(|s| s.contains("pong from rcon"))),
        "expected command output in {doc}"
    );
    assert!(out.iter().any(|line| {
        line.as_str()
            .is_some_and(|s| s.contains("Built-in commands"))
    }));

    lb2.fx.server.execute("set rcon_password \"\"");
    let (disabled, _) = send_rcon(&lb2, "X-Rcon-Password: super-secret-pw\r\n", "help");
    assert_eq!(disabled, 403);
}

#[test]
fn watch_resources_is_off_by_default_and_console_only() {
    let fx = Fixture::new();
    fx.add_resource(
        "helper",
        &manifest(&["server.lua"], &[], &[]),
        &[("server.lua", "ExecuteCommand('watch_resources on')\n")],
    );
    fx.server
        .execute("add_ace resource.helper command.watch_resources allow");

    assert!(!fx.manager().watch_enabled());
    fx.server.execute("ensure helper");
    assert!(
        !fx.manager().watch_enabled(),
        "resources must not be able to toggle watch_resources"
    );

    fx.server.execute("watch_resources on");
    assert!(fx.manager().watch_enabled());

    fx.server.execute("watch_resources off");
    assert!(!fx.manager().watch_enabled());
}

fn rcon_output(lb: &Loopback, command: &str) -> Vec<String> {
    let (status, body) = send_rcon(lb, "X-Rcon-Password: super-secret-pw\r\n", command);
    assert_eq!(status, 200, "rcon `{command}` failed");
    let doc: Json = serde_json::from_slice(&body).expect("rcon json");
    assert_eq!(doc["ok"], true);
    doc["output"]
        .as_array()
        .expect("output array")
        .iter()
        .filter_map(|line| line.as_str().map(str::to_owned))
        .collect()
}

#[test]
fn status_reports_server_resources_and_connected_players() {
    let lb = Loopback::new(&[RCON_TEST_RESOURCE]);
    lb.fx.server.execute("set rcon_password super-secret-pw");
    lb.fx.server.execute(r#"set sv_hostname "D1 Test Server""#);
    lb.fx.server.execute("set sv_maxclients 16");

    let _alice = lb.connect("Alice").expect("Alice connected");
    let _bob = lb.connect("Bob").expect("Bob connected");

    let extra = rcon_output(&lb, "status extra");
    assert!(
        extra.iter().any(|line| line.contains("usage: status")),
        "expected usage error: {extra:?}"
    );

    let out = rcon_output(&lb, "status");
    assert!(
        out.iter()
            .any(|line| line == "server: D1 Test Server, resources: 1/1, players: 2/16"),
        "expected status summary in {out:?}"
    );
    let alice_idx = out
        .iter()
        .position(|line| {
            line.contains("Alice") && line.contains("ping ") && line.contains("license:")
        })
        .expect("Alice row in status");
    let bob_idx = out
        .iter()
        .position(|line| {
            line.contains("Bob") && line.contains("ping ") && line.contains("license:")
        })
        .expect("Bob row in status");
    assert!(alice_idx < bob_idx, "expected join order: {out:?}");
}

#[test]
fn clientkick_drops_players_with_origin_and_checks_ace() {
    let kicker = Resource {
        name: "kicker",
        script: r"
            local drops = {}
            AddEventHandler('playerDropped', function(reason, resource, code)
                drops[#drops + 1] = ('%s|%s|%s|%s'):format(source, reason, resource, code)
            end)
            RegisterCommand('relay_kick', function(_, args)
                ExecuteCommand('clientkick ' .. table.concat(args, ' '))
            end)
            RegisterCommand('marker', function(src)
                drops[#drops + 1] = ('marker:%s'):format(src)
            end, false)
            function log()
                return table.concat(drops, '\n')
            end
        ",
        exports: &["log"],
    };
    let lb = Loopback::new(&[kicker]);
    lb.fx.server.execute("set rcon_password super-secret-pw");

    let alice = lb.connect("Alice").expect("Alice connected");
    let bob = lb.connect("Bob").expect("Bob connected");
    let carol = lb.connect("Carol").expect("Carol connected");

    let bad_usage = rcon_output(&lb, "clientkick 0");
    assert!(
        bad_usage
            .iter()
            .any(|line| line.contains("usage: clientkick <id> [reason]")),
        "expected usage line: {bad_usage:?}"
    );
    let unknown = rcon_output(&lb, "clientkick 999");
    assert!(
        unknown
            .iter()
            .any(|line| line.contains("clientkick: unknown player 999")),
        "expected unknown player warning: {unknown:?}"
    );

    // Console kick with omitted reason uses default reason and DROP_SERVER (2).
    lb.fx.server.execute("clientkick 1");
    let reason = lb.wait_for("Alice kick", || match alice.recv_timeout(STEP) {
        Some(ClientEvent::Kicked { reason }) => Some(reason),
        _ => None,
    });
    assert_eq!(reason, "Kicked from server");
    lb.wait_for_log("1|Kicked from server||2");

    // Resource kick requires `command.clientkick` ACE and reports DROP_SCRIPT (1).
    lb.fx.server.execute("relay_kick 2 denied");
    for _ in 0..10 {
        lb.pump();
    }
    assert!(lb.fx.manager().server().unwrap().players().contains(&2));

    lb.fx
        .server
        .execute("add_ace resource.kicker command.clientkick allow");
    lb.fx.server.execute("relay_kick 2 rule violation");
    let bob_reason = lb.wait_for("Bob kick", || match bob.recv_timeout(STEP) {
        Some(ClientEvent::Kicked { reason }) => Some(reason),
        _ => None,
    });
    assert_eq!(bob_reason, "rule violation");
    lb.wait_for_log("2|rule violation|kicker|1");

    // Player kick requires `command.clientkick` ACE and reports DROP_SERVER (2).
    let dan = lb.connect("Dan").expect("Dan connected");
    carol
        .send_command("clientkick 4 denied")
        .expect("send denied clientkick");
    carol.send_command("marker").expect("send marker");
    lb.wait_for_log("marker:3");
    assert!(lb.fx.manager().server().unwrap().players().contains(&4));

    lb.fx
        .server
        .execute("add_ace player.3 command.clientkick allow");
    carol
        .send_command("clientkick 4 by player")
        .expect("send allowed clientkick");
    let dan_reason = lb.wait_for("Dan kick", || match dan.recv_timeout(STEP) {
        Some(ClientEvent::Kicked { reason }) => Some(reason),
        _ => None,
    });
    assert_eq!(dan_reason, "by player");
    lb.wait_for_log("4|by player||2");
}

const SAY_PROBE_SCRIPT: &str = r"
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

const CHAT_SERVER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/server-data/resources/[system]/chat/server.lua"
));

#[test]
fn builtin_say_logs_without_chat_and_validates_input() {
    let no_chat = Loopback::new(&[Resource {
        name: "probe",
        script: SAY_PROBE_SCRIPT,
        exports: &["log"],
    }]);
    no_chat
        .fx
        .server
        .execute("set rcon_password super-secret-pw");
    let _viewer = no_chat.connect("Viewer").expect("Viewer connected");

    let empty = rcon_output(&no_chat, "say   ");
    assert!(
        empty.iter().any(|line| line.contains("usage: say <text>")),
        "expected usage: {empty:?}"
    );
    let long_cmd = format!("say {}", "x".repeat(257));
    let too_long = rcon_output(&no_chat, &long_cmd);
    assert!(
        too_long
            .iter()
            .any(|line| line.contains("say: message exceeds 256 bytes")),
        "expected length warning: {too_long:?}"
    );
    let plain = rcon_output(&no_chat, "say hello without chat");
    assert!(
        plain
            .iter()
            .any(|line| line == "Console: hello without chat"),
        "expected plain console log: {plain:?}"
    );
    assert_eq!(no_chat.log(), "");
}

#[test]
fn builtin_say_integrates_with_chat_and_respects_cancellation() {
    let with_chat = Loopback::new(&[
        Resource {
            name: "probe",
            script: SAY_PROBE_SCRIPT,
            exports: &["log"],
        },
        Resource {
            name: "chat",
            script: CHAT_SERVER,
            exports: &[],
        },
    ]);
    with_chat
        .fx
        .server
        .execute("set rcon_password super-secret-pw");
    let alice = with_chat.connect("Alice").expect("Alice connected");

    let canceled = rcon_output(&with_chat, "say blocked");
    assert!(
        !canceled
            .iter()
            .any(|line| line.contains("Console: blocked")),
        "canceled message must not log: {canceled:?}"
    );
    with_chat.wait_for_log("0|Console|blocked");

    let deadline = Instant::now() + Duration::from_millis(80);
    while Instant::now() < deadline {
        with_chat.pump();
        if let Some(ClientEvent::Event { name, args }) = alice.recv_timeout(STEP) {
            assert_ne!(name, "chat:addMessage", "canceled broadcast: {args:?}");
        }
    }

    let ok = rcon_output(&with_chat, "say hello chat");
    assert!(
        ok.iter().any(|line| line == "Console: hello chat"),
        "expected chat log: {ok:?}"
    );
    with_chat.wait_for_log("0|Console|hello chat");
    let args = with_chat.expect_event(&alice, "chat:addMessage");
    assert_eq!(
        args,
        vec![Value::Map(vec![(
            Value::from("args"),
            Value::Array(vec![Value::from("Console"), Value::from("hello chat")]),
        )])]
    );

    with_chat
        .fx
        .server
        .execute("add_ace builtin.everyone command.say allow");
    alice
        .send_command("say   from\tplayer  ")
        .expect("send player say");
    with_chat.wait_for_log("1|Alice|from player");
    let player_args = with_chat.expect_event(&alice, "chat:addMessage");
    assert_eq!(
        player_args,
        vec![Value::Map(vec![(
            Value::from("args"),
            Value::Array(vec![Value::from("Alice"), Value::from("from player")]),
        )])]
    );
}

#[test]
fn cmdlist_lists_sorted_builtins_and_script_commands() {
    let cmds = Resource {
        name: "cmds",
        script: r"
            RegisterCommand('zeta_cmd', function() end, true)
            RegisterCommand('alpha_cmd', function() end, false)
        ",
        exports: &[],
    };
    let lb = Loopback::new(&[cmds]);
    lb.fx.server.execute("set rcon_password super-secret-pw");

    let bad = rcon_output(&lb, "cmdlist extra");
    assert!(
        bad.iter().any(|line| line.contains("usage: cmdlist")),
        "expected usage line: {bad:?}"
    );

    let out = rcon_output(&lb, "cmdlist");
    for builtin in ["clientkick", "cmdlist", "exec", "help", "say", "status"] {
        assert!(
            out.iter().any(|line| line == builtin),
            "missing builtin `{builtin}` in {out:?}"
        );
    }
    let idx_clientkick = out.iter().position(|l| l == "clientkick").unwrap();
    let idx_cmdlist = out.iter().position(|l| l == "cmdlist").unwrap();
    let idx_say = out.iter().position(|l| l == "say").unwrap();
    let idx_status = out.iter().position(|l| l == "status").unwrap();
    assert!(idx_clientkick < idx_cmdlist && idx_cmdlist < idx_say && idx_say < idx_status);

    let idx_alpha = out
        .iter()
        .position(|l| l == "alpha_cmd")
        .expect("alpha_cmd");
    let idx_zeta = out
        .iter()
        .position(|l| l == "zeta_cmd (restricted)")
        .expect("zeta_cmd (restricted)");
    assert!(
        idx_alpha < idx_zeta,
        "expected sorted script commands: {out:?}"
    );
}

#[test]
fn exec_at_resource_supports_aliases_nesting_ace_and_rejects_traversal() {
    let fx = Fixture::new();
    fx.add_resource(
        "impl_cfg",
        "fx_version 'cerulean'\ngame 'common'\nprovide 'alias_cfg'\n",
        &[
            (
                "cfg/main.cfg",
                "set from_alias ok\nexec @alias_cfg/cfg/nested.cfg\n",
            ),
            ("cfg/nested.cfg", "set from_nested yes\n"),
            (
                "cfg/self.cfg",
                "set depth_hit 1\nexec @alias_cfg/cfg/self.cfg\n",
            ),
            ("cfg/priv.cfg", "set ace_cfg allowed\nquit\n"),
        ],
    );
    fx.add_resource(
        "driver",
        &manifest(&["server.lua"], &[], &[]),
        &[(
            "server.lua",
            "RegisterCommand('run_exec', function(_, args) ExecuteCommand('exec ' .. args[1]) end)\n",
        )],
    );

    // Works before any `start`/`ensure`/`refresh` via `provide` alias and nested `@` exec.
    fx.server.execute("exec @alias_cfg/cfg/main.cfg");
    assert_eq!(fx.manager().convar("from_alias").as_deref(), Some("ok"));
    assert_eq!(fx.manager().convar("from_nested").as_deref(), Some("yes"));

    // Self-referential `@` exec stops at MAX_EXEC_DEPTH.
    fx.server.execute("exec @alias_cfg/cfg/self.cfg");
    assert_eq!(fx.manager().convar("depth_hit").as_deref(), Some("1"));

    // ACE preservation across resource `ExecuteCommand('exec @...')`.
    fx.server.execute("ensure driver");
    fx.server.execute("run_exec @alias_cfg/cfg/priv.cfg");
    assert_eq!(fx.manager().convar("ace_cfg"), None);

    fx.server
        .execute("add_ace resource.driver command.exec allow");
    fx.server.execute("run_exec @alias_cfg/cfg/priv.cfg");
    assert_eq!(fx.manager().convar("ace_cfg"), None);
    assert!(!fx.manager().quit_requested());

    fx.server
        .execute("add_ace resource.driver command.set allow");
    fx.server.execute("run_exec @alias_cfg/cfg/priv.cfg");
    assert_eq!(fx.manager().convar("ace_cfg").as_deref(), Some("allowed"));
    assert!(!fx.manager().quit_requested());

    // Traversal, invalid forms, and symlinks are rejected.
    let outside = fx.root().join("outside.cfg");
    write_file(&outside, "set pwned yes\n");
    for bad in [
        "exec @alias_cfg/../outside.cfg",
        "exec @alias_cfg/../../outside.cfg",
        "exec @alias_cfg//cfg/main.cfg",
        "exec @alias_cfg",
        "exec @alias_cfg/",
        "exec @/cfg/main.cfg",
        "exec @unknown_res/cfg/main.cfg",
        "exec @alias_cfg/CON.cfg",
    ] {
        fx.server.execute(bad);
    }
    #[cfg(unix)]
    {
        let link = fx.resources().join("impl_cfg/cfg/link.cfg");
        std::os::unix::fs::symlink(&outside, &link).expect("create symlink");
        fx.server.execute("exec @alias_cfg/cfg/link.cfg");
    }
    assert_eq!(fx.manager().convar("pwned"), None);
}
