//! Integration tests for FiveM compatibility natives: backtick hash literals,
//! `quat` wire/export round-trips, `GetEntityQuaternion`/`SetEntityQuaternion`,
//! `AddConvarChangeListener`/`RemoveConvarChangeListener`, `GetRegisteredCommands`,
//! `StartResource`/`StopResource` ACE gating, `GetEntityScript`,
//! `NetworkGetFirstEntityOwner`, `SetEntityDistanceCullingRadius`,
//! `GetPlayerLastMsg`, and `GetNumPlayerTokens`/`GetPlayerToken`.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_client::{Client, ClientRuntime};
use sixm_core::{ClientHost, ConvarKind, Host, ServerHost, Value};
use sixm_net::Identity;
use sixm_server::Input;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(20);
const STEP: Duration = Duration::from_millis(5);

struct Loopback {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
}

impl Loopback {
    fn new(name: &str, manifest: &str, files: &[(&str, &str)]) -> Self {
        let fx = Fixture::new();
        fx.add_resource(name, manifest, files);
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (tx, input) = mpsc::channel();
        let addr = fx.server.start_network(tx).expect("loopback server");
        fx.server.execute(&format!("ensure {name}"));
        Self { fx, input, addr }
    }

    fn pump_server(&self) {
        while let Ok(input) = self.input.try_recv() {
            self.fx.server.handle(input);
        }
        self.fx.server.tick(Instant::now());
    }

    fn connect(&self, name: &str, cache: &Path) -> Client {
        let addr = self.addr;
        let name = name.to_owned();
        let cache = cache.to_path_buf();
        let handle = thread::spawn(move || {
            Client::connect(addr, &name, None, &Identity::Ephemeral, cache, TIMEOUT)
        });
        let deadline = Instant::now() + TIMEOUT;
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "client connect timed out");
            match self.input.recv_timeout(STEP) {
                Ok(input) => self.fx.server.handle(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle
            .join()
            .expect("client connect thread")
            .expect("client synced")
    }

    fn pump_both(&self, c1: &ClientRuntime, c2: Option<&ClientRuntime>, now: Instant) {
        self.pump_server();
        c1.pump(Duration::ZERO);
        c1.tick(now);
        if let Some(c2) = c2 {
            c2.pump(Duration::ZERO);
            c2.tick(now);
        }
        self.pump_server();
    }
}

const SRV_COMPAT_LUA: &str = r"
local convar_events = {}
local cookie = AddConvarChangeListener('game_*', function(name)
    convar_events[#convar_events + 1] = name
end)

RegisterCommand('compat_ping', function() end, false)

function summary()
    local cmds = GetRegisteredCommands()
    local has_start, has_ping = false, false
    for _, c in ipairs(cmds) do
        if c.name == 'start' then has_start = true end
        if c.name == 'compat_ping' then has_ping = true end
    end
    local pid = GetPlayers()[1]
    return {
        convar_events = table.concat(convar_events, ','),
        has_start = has_start,
        has_ping = has_ping,
        hash_ok = (`adder` == GetHashKey('adder')),
        denied_start = StartResource('extra_res'),
        last_msg_valid = pid ~= nil and GetPlayerLastMsg(pid) >= 0,
        num_tokens = pid ~= nil and GetNumPlayerTokens(pid) or 0,
        token0 = pid ~= nil and GetPlayerToken(pid, 0) or nil,
        cookie = cookie,
    }
end

exports('stop_listener', function()
    RemoveConvarChangeListener(cookie)
end)

exports('manage_extra', function()
    local started = StartResource('extra_res')
    local state_after_start = GetResourceState('extra_res')
    local stopped = StopResource('extra_res')
    local state_after_stop = GetResourceState('extra_res')
    return started, state_after_start, stopped, state_after_stop
end)

local cli_log = {}
RegisterNetEvent('compat:clientConvar', function(name)
    cli_log[#cli_log + 1] = 'cv:' .. name
end)
RegisterNetEvent('compat:commands', function(ok)
    cli_log[#cli_log + 1] = 'cmd:' .. tostring(ok)
end)
RegisterNetEvent('compat:quatCheck', function(q)
    cli_log[#cli_log + 1] = ('quat:%.2f,%.2f,%.2f,%.2f'):format(q.w, q.x, q.y, q.z)
end)
function client_log() return table.concat(cli_log, '|') end
";

const CLI_COMPAT_JS: &str = r#"
const seen = [];
let listener = AddConvarChangeListener("game_*", (name) => {
    seen.push(name);
    emitNet("compat:clientConvar", name);
});
RegisterCommand("client_cmd", () => {}, false);
onNet("compat:unsubConvar", () => {
    RemoveConvarChangeListener(listener);
});
RegisterNetEvent("compat:checkCommands", () => {
    const names = GetRegisteredCommands().map((c) => c.name);
    emitNet("compat:commands", names.includes("client_cmd") && names.includes("help"));
});
onNet("compat:sendQuat", () => {
    emitNet("compat:quatCheck", quat(0.5, -0.5, 0.25, -0.25));
});
"#;

const SRV_ONESYNC_LUA: &str = r"
function setup()
    local server_veh = CreateVehicle(`adder`, 800.0, 0.0, 0.0, 45.0, true, false)
    SetEntityDistanceCullingRadius(server_veh, 1200.0)
    return server_veh, GetEntityScript(server_veh), NetworkGetFirstEntityOwner(server_veh)
end

function transfer(client_ent, new_owner)
    local first_before = NetworkGetFirstEntityOwner(client_ent)
    SetEntityOwner(client_ent, new_owner)
    local first_after = NetworkGetFirstEntityOwner(client_ent)
    return first_before, first_after, NetworkGetEntityOwner(client_ent)
end

function adjust_cull(ent, r)
    SetEntityDistanceCullingRadius(ent, r)
end
";

const CLI_ONESYNC_LUA: &str = r"
exports('spawn_local', function()
    local e = CreateVehicle(`adder`, 0.0, 0.0, 0.0, 0.0, true)
    return e, GetEntityScript(e)
end)
exports('knows', function(id)
    return DoesEntityExist(id)
end)
";

fn pump_for(lb: &Loopback, client: &ClientRuntime, ticks: usize) {
    for _ in 0..ticks {
        lb.pump_both(client, None, Instant::now());
        thread::sleep(STEP);
    }
}

fn verify_summary_and_resources(lb: &Loopback) {
    let res = lb
        .fx
        .manager()
        .call_export("compat", "summary", &[], None)
        .expect("summary");
    let Value::Map(entries) = &res[0] else {
        panic!("expected map, got {res:?}");
    };
    let field = |k: &str| {
        entries
            .iter()
            .find(|(ek, _)| ek.as_str() == Some(k))
            .map(|(_, v)| v.clone())
            .expect(k)
    };
    assert_eq!(field("convar_events"), Value::from("game_mode"));
    assert_eq!(field("has_start"), Value::Bool(true));
    assert_eq!(field("has_ping"), Value::Bool(true));
    assert_eq!(field("hash_ok"), Value::Bool(true));
    assert_eq!(field("denied_start"), Value::Bool(false));
    assert_eq!(field("last_msg_valid"), Value::Bool(true));
    assert_eq!(field("num_tokens"), Value::Int(1));
    let Value::String(token) = field("token0") else {
        panic!("expected token string");
    };
    assert!(String::from_utf8_lossy(token.as_ref()).starts_with("0:"));

    lb.fx
        .server
        .execute("add_ace resource.compat command.start allow");
    lb.fx
        .server
        .execute("add_ace resource.compat command.stop allow");
    let managed = lb
        .fx
        .manager()
        .call_export("compat", "manage_extra", &[], None)
        .expect("manage_extra");
    assert_eq!(
        managed,
        vec![
            Value::Bool(true),
            Value::from("started"),
            Value::Bool(true),
            Value::from("stopped"),
        ]
    );
}

#[test]
fn convar_listeners_commands_resources_and_player_tokens_work_end_to_end() {
    let mut srv_map = manifest(
        &["server.lua"],
        &["summary", "client_log", "stop_listener", "manage_extra"],
        &[],
    );
    srv_map.push_str("client_script 'client.js'\n");

    let lb = Loopback::new(
        "compat",
        &srv_map,
        &[("server.lua", SRV_COMPAT_LUA), ("client.js", CLI_COMPAT_JS)],
    );
    lb.fx.add_resource(
        "extra_res",
        "fx_version 'cerulean'\ngame 'gta6'\nserver_script 'extra.lua'\n",
        &[("extra.lua", "")],
    );
    lb.fx.server.execute("refresh");

    let cache = TempDir::new().expect("cache");
    let client = ClientRuntime::new(lb.connect("Alice", cache.path()), cache.path());
    client.start_synced();

    lb.fx
        .manager()
        .set_convar("other_var", "x", ConvarKind::Plain)
        .expect("set plain");
    lb.fx
        .manager()
        .set_convar("game_mode", "race", ConvarKind::Replicated)
        .expect("setr game_mode");

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        lb.pump_both(&client, None, Instant::now());
        if let Value::String(log) = &lb
            .fx
            .manager()
            .call_export("compat", "client_log", &[], None)
            .expect("client_log")[0]
            && String::from_utf8_lossy(log.as_ref()).contains("cv:game_mode")
        {
            break;
        }
        thread::sleep(STEP);
    }

    lb.fx
        .manager()
        .call_export("compat", "stop_listener", &[], None)
        .expect("stop_listener");
    lb.fx
        .manager()
        .trigger_client_event("compat:unsubConvar", sixm_core::ClientTarget::All, &[])
        .expect("unsub");
    lb.fx
        .manager()
        .trigger_client_event("compat:checkCommands", sixm_core::ClientTarget::All, &[])
        .expect("checkCommands");
    lb.fx
        .manager()
        .trigger_client_event("compat:sendQuat", sixm_core::ClientTarget::All, &[])
        .expect("sendQuat");
    pump_for(&lb, &client, 20);
    lb.fx
        .manager()
        .set_convar("game_round", "2", ConvarKind::Replicated)
        .expect("setr game_round");
    pump_for(&lb, &client, 10);

    verify_summary_and_resources(&lb);

    let log = &lb
        .fx
        .manager()
        .call_export("compat", "client_log", &[], None)
        .expect("client_log")[0];
    let log_str = log.as_str().expect("log string");
    assert!(log_str.contains("cv:game_mode"), "{log_str}");
    assert!(!log_str.contains("game_round"), "{log_str}");
    assert!(log_str.contains("cmd:true"), "{log_str}");
    assert!(log_str.contains("quat:0.50,-0.50,0.25,-0.25"), "{log_str}");
}

#[test]
fn onesync_entity_natives_cull_radius_and_first_owner_work() {
    let mut srv_manifest = manifest(&["server.lua"], &["setup", "transfer", "adjust_cull"], &[]);
    srv_manifest.push_str("client_script 'client.lua'\n");

    let lb = Loopback::new(
        "onesync_test",
        &srv_manifest,
        &[
            ("server.lua", SRV_ONESYNC_LUA),
            ("client.lua", CLI_ONESYNC_LUA),
        ],
    );
    lb.fx.server.execute("set sixm_cullRadius 200");

    let cache_a = TempDir::new().expect("cache a");
    let cache_b = TempDir::new().expect("cache b");
    let alice = ClientRuntime::new(lb.connect("Alice", cache_a.path()), cache_a.path());
    let bob = ClientRuntime::new(lb.connect("Bob", cache_b.path()), cache_b.path());
    alice.start_synced();
    bob.start_synced();

    // Server creates a distant vehicle at x=800 (global radius is 200) and gives it cull radius 1200.
    let setup = lb
        .fx
        .manager()
        .call_export("onesync_test", "setup", &[], None)
        .expect("setup");
    let Value::Int(server_veh) = setup[0] else {
        panic!("expected id");
    };
    assert_eq!(setup[1], Value::from("onesync_test"));
    assert_eq!(setup[2], Value::Int(-1));

    for _ in 0..10 {
        lb.pump_both(&alice, Some(&bob), Instant::now());
        thread::sleep(STEP);
    }

    // Both Alice (at 0) and Bob (at 0) see the distant vehicle because its custom cull radius is 1200.
    assert_eq!(
        alice
            .call_export("onesync_test", "knows", &[Value::Int(server_veh)], None)
            .expect("alice knows"),
        vec![Value::Bool(true)]
    );

    // Shrink custom cull radius to 100 -> out of Alice's scope (distance 800 > 100).
    lb.fx
        .manager()
        .call_export(
            "onesync_test",
            "adjust_cull",
            &[Value::Int(server_veh), Value::Float(100.0)],
            None,
        )
        .expect("shrink cull");
    for _ in 0..10 {
        lb.pump_both(&alice, Some(&bob), Instant::now());
        thread::sleep(STEP);
    }
    assert_eq!(
        alice
            .call_export("onesync_test", "knows", &[Value::Int(server_veh)], None)
            .expect("alice no longer knows"),
        vec![Value::Bool(false)]
    );

    // Alice creates a client entity -> check GetEntityScript on client and NetworkGetFirstEntityOwner across SetEntityOwner.
    let spawned = alice
        .call_export("onesync_test", "spawn_local", &[], None)
        .expect("spawn_local");
    assert_eq!(spawned[1], Value::from("onesync_test"));
    let Value::Int(client_ent) = spawned[0] else {
        panic!("expected client_ent id");
    };
    for _ in 0..10 {
        lb.pump_both(&alice, Some(&bob), Instant::now());
        thread::sleep(STEP);
    }
    let alice_id = i64::from(alice.local_player());
    let bob_id = i64::from(bob.local_player());
    let transfer = lb
        .fx
        .manager()
        .call_export(
            "onesync_test",
            "transfer",
            &[Value::Int(client_ent), Value::Int(bob_id)],
            None,
        )
        .expect("transfer");
    assert_eq!(
        transfer,
        vec![
            Value::Int(alice_id),
            Value::Int(alice_id),
            Value::Int(bob_id),
        ]
    );
}
