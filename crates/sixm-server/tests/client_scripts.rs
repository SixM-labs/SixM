//! Client scripts end to end: a server on loopback runs a resource with client
//! scripts, and a `ClientRuntime` syncs it and runs those scripts.
//!
//! The test thread plays both loops. Server inputs are handled between client
//! pumps, and the client's reports come back to the server as events, where the
//! server resource keeps them for the test to read.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_client::{Client, ClientRuntime};
use sixm_core::{ConvarKind, GLOBAL_BAG, Host, Value};
use sixm_net::Identity;
use sixm_server::Input;
use tempfile::TempDir;

/// Longest a test waits for the server or the client to do something.
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long each wait step blocks before checking again.
const STEP: Duration = Duration::from_millis(5);
/// The resource the tests start. Its name matches the example server.
const RESOURCE: &str = "hello";

/// Server side: records what the client reports, and sends the client events.
const SERVER_SCRIPT: &str = r"
local lines = {}

local function record(line)
    lines[#lines + 1] = line
end

GlobalState.probe = 'ready'

RegisterNetEvent('hello:report', function(text)
    record(('%d: %s'):format(source, text))
end)

RegisterNetEvent('hello:ping', function(text)
    record(('%d: ping %s'):format(source, text))
    TriggerClientEvent('hello:plain', source, 'no RegisterNetEvent on the client')
    TriggerClientEvent('hello:pong', source, 'pong for ' .. text)
end)

function report()
    return table.concat(lines, '\n')
end

function poke()
    GlobalState.probe = 'changed'
end
";

/// Client side: reports what it sees through `hello:report`.
const CLIENT_SCRIPT: &str = r"
local function report(text)
    TriggerServerEvent('hello:report', text)
end

report(('duplicity %s'):format(tostring(IsDuplicityVersion())))
report(('probe at load %s'):format(tostring(GlobalState.probe)))

RegisterNetEvent('hello:pong', function(text)
    report(('pong %d %s'):format(source, text))
end)

AddEventHandler('hello:plain', function(text)
    report('plain ' .. text)
end)

AddEventHandler('onClientResourceStart', function(name)
    if name == GetCurrentResourceName() then
        report('started')
    end
end)

AddEventHandler('onClientResourceStop', function(name)
    if name == GetCurrentResourceName() then
        report('stopped')
    end
end)

AddStateBagChangeHandler('probe', nil, function(bag, key, value, _, replicated)
    report(('change %s.%s %s readable %s replicated %s'):format(
        bag, key, tostring(value), tostring(GlobalState.probe), tostring(replicated)))
end)

local refused = not pcall(function() GlobalState.probe = 'client' end)
report(('replicated write refused %s'):format(tostring(refused)))

LocalPlayer.state:set('mine', 'yes', false)
report('local ' .. tostring(LocalPlayer.state.mine))

RegisterCommand('probe', function(_, args)
    report('command ' .. args[1])
end)

TriggerServerEvent('hello:ping', 'from the client')
";

/// Client side of the file test: reads and writes through `LoadResourceFile`
/// and `SaveResourceFile`, and reports each result.
const FILES_CLIENT_SCRIPT: &str = r"
local name = GetCurrentResourceName()

TriggerServerEvent('hello:report', 'config ' .. tostring(LoadResourceFile(name, 'data/config.json')))
TriggerServerEvent('hello:report', 'save declared ' .. tostring(SaveResourceFile(name, 'data/config.json', 'x')))
TriggerServerEvent('hello:report', 'save own ' .. tostring(SaveResourceFile(name, 'notes.txt', 'mine')))
TriggerServerEvent('hello:report', 'load own ' .. tostring(LoadResourceFile(name, 'notes.txt')))
TriggerServerEvent('hello:report', 'missing ' .. tostring(LoadResourceFile(name, 'nope.txt')))
";

/// Provider side of the export test: declares `greet` and defines it, and also
/// defines `notDeclared`, which no manifest declares.
const PROVIDER_CLIENT_SCRIPT: &str = r"
function greet(name)
    return 'hello ' .. name
end

function notDeclared()
    return 'reachable'
end
";

/// Consumer server side of the export test: records the consumer's reports.
const CONSUMER_SERVER_SCRIPT: &str = r"
local lines = {}

RegisterNetEvent('consumer:report', function(text)
    lines[#lines + 1] = ('%d: %s'):format(source, text)
end)

function report()
    return table.concat(lines, '\n')
end
";

/// Consumer client side of the export test: calls the provider's exports as
/// soon as it starts, after the provider has loaded.
const CONSUMER_CLIENT_SCRIPT: &str = r"
AddEventHandler('onClientResourceStart', function(name)
    if name ~= GetCurrentResourceName() then
        return
    end
    local ok, greeting = pcall(function() return exports.provider:greet('bob') end)
    TriggerServerEvent('consumer:report', ('greet %s %s'):format(tostring(ok), tostring(greeting)))
    local reached = pcall(function() return exports.provider:notDeclared() end)
    TriggerServerEvent('consumer:report', ('undeclared %s'):format(tostring(reached)))
end)
";

/// Client side of the resource natives test: reports its own state and
/// metadata while it loads and once it has started, and the resources it knows.
const RESOURCE_INFO_CLIENT_SCRIPT: &str = r"
local name = GetCurrentResourceName()

TriggerServerEvent('hello:report', 'state ' .. GetResourceState(name))
TriggerServerEvent('hello:report', 'version ' .. tostring(GetResourceMetadata(name, 'version', 0)))
TriggerServerEvent('hello:report', 'count ' .. GetNumResources() .. ' ' .. tostring(GetResourceByFindIndex(0)))

AddEventHandler('onClientResourceStart', function(started)
    if started == name then
        TriggerServerEvent('hello:report', 'state ' .. GetResourceState(name))
    end
end)
";

/// A server listening on loopback, whose inputs are handled by the test thread.
struct Harness {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
    /// The resource whose `report` export the test reads.
    reporter: &'static str,
}

impl Harness {
    /// A server with its network running on loopback and no resource started.
    /// `reporter` is the resource whose `report` export holds the log.
    fn bare(reporter: &'static str) -> Self {
        let fx = Fixture::new();
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        Self {
            fx,
            input,
            addr,
            reporter,
        }
    }

    /// A server with the `hello` resource started and its network running.
    fn new() -> Self {
        let h = Self::bare(RESOURCE);
        let text = manifest(&["server.lua"], &["report", "poke"], &[])
            + "client_scripts { 'client.lua' }\n";
        h.add(
            RESOURCE,
            &text,
            &[("server.lua", SERVER_SCRIPT), ("client.lua", CLIENT_SCRIPT)],
        );
        h.start(RESOURCE);
        h
    }

    /// Writes resource `name`. The catalog is scanned once, when the first
    /// resource starts, so every resource must be added before that.
    fn add(&self, name: &str, text: &str, files: &[(&str, &str)]) {
        self.fx.add_resource(name, text, files);
    }

    /// Starts resource `name`, after the resources it depends on.
    fn start(&self, name: &str) {
        self.fx.server.execute(&format!("start {name}"));
    }

    /// Handles every queued server input.
    fn pump_server(&self) {
        while let Ok(input) = self.input.try_recv() {
            self.fx.server.handle(input);
        }
    }

    /// Connects a client that keeps its files in `cache` and its data in
    /// `data`, then starts the resources it synced.
    fn connect(&self, cache: &Path, data: &Path) -> Rc<ClientRuntime> {
        let runtime = ClientRuntime::new(self.connect_client(cache), data);
        runtime.start_synced();
        runtime
    }

    /// Connects a client that keeps its files in `cache`, and returns it
    /// without wrapping it in a runtime. Its messages are not read until
    /// something polls it.
    fn connect_client(&self, cache: &Path) -> Client {
        let addr = self.addr;
        let cache = cache.to_path_buf();
        let handle = thread::spawn(move || {
            Client::connect(addr, "tester", None, &Identity::Ephemeral, cache, TIMEOUT)
        });
        let deadline = Instant::now() + TIMEOUT;
        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "the client did not connect in time"
            );
            match self.input.recv_timeout(STEP) {
                Ok(input) => self.fx.server.handle(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle
            .join()
            .expect("the client thread does not panic")
            .expect("the client connects and syncs")
    }

    /// Handles server inputs until the report log satisfies `done`, and returns
    /// the log. For a client that is not polled.
    fn wait_for_server_log(&self, what: &str, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match self.input.recv_timeout(STEP) {
                Ok(input) => self.fx.server.handle(input),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
            let log = self.fx.text(self.reporter, "report");
            if done(&log) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the server has:\n{log}"
            );
        }
    }

    /// Runs both loops until the server's report log satisfies `done`, and
    /// returns the log.
    fn wait_for_report(
        &self,
        runtime: &ClientRuntime,
        what: &str,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump_server();
            runtime.pump(STEP);
            runtime.tick(Instant::now());
            let log = self.fx.text(self.reporter, "report");
            if done(&log) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the server has:\n{log}"
            );
        }
    }
}

/// A connected client with its resource started.
struct Session {
    h: Harness,
    runtime: Rc<ClientRuntime>,
    _dir: TempDir,
}

fn session() -> Session {
    let h = Harness::new();
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    Session {
        h,
        runtime,
        _dir: dir,
    }
}

/// Whether the report log has a line exactly `line`.
fn has_line(log: &str, line: &str) -> bool {
    log.lines().any(|candidate| candidate == line)
}

#[test]
fn client_script_runs_on_join_and_its_server_event_reaches_the_handler() {
    let s = session();
    let log = s.h.wait_for_report(&s.runtime, "the ping", |log| {
        has_line(log, "1: ping from the client") && has_line(log, "1: started")
    });
    assert!(has_line(&log, "1: started"), "{log}");
}

#[test]
fn latent_payload_larger_than_a_frame_round_trips_through_lua() {
    let h = Harness::bare("latent");
    let text = manifest(&["server.lua"], &["report"], &[]) + "client_script 'client.lua'\n";
    let server = r"
        local result = ''
        RegisterNetEvent('upload', function(bytes)
            TriggerLatentClientEvent('echo', source, 16000000, bytes, source)
        end)
        RegisterNetEvent('result', function(text) result = text end)
        function report() return result end
    ";
    let client = r"
        local payload = string.rep('x', 1024 * 1024 + 1)
        RegisterNetEvent('echo', function(bytes, sender)
            TriggerServerEvent('result', ('%s/%s/%s'):format(bytes == payload, sender, source))
        end)
        TriggerLatentServerEvent('upload', 16000000, payload)
    ";
    h.add(
        "latent",
        &text,
        &[("server.lua", server), ("client.lua", client)],
    );
    h.start("latent");
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    h.wait_for_report(&runtime, "the complete Lua echo", |log| {
        log == "true/1/65535"
    });
}

#[test]
fn server_event_reaches_client_handler_with_source_65535() {
    let s = session();
    s.h.wait_for_report(&s.runtime, "the pong", |log| {
        has_line(log, "1: pong 65535 pong for from the client")
    });
}

#[test]
fn client_handler_without_register_net_event_gets_no_server_events() {
    let s = session();
    let log = s.h.wait_for_report(&s.runtime, "the pong", |log| {
        has_line(log, "1: pong 65535 pong for from the client")
    });
    assert!(!log.lines().any(|line| line.contains("plain")), "{log}");
}

#[test]
fn replicated_global_state_is_readable_and_reaches_client_change_handlers() {
    let s = session();
    // The state is applied before the scripts load, so the script reads it at
    // once, and its change handler is only called for later changes.
    let log = s.h.wait_for_report(&s.runtime, "the initial state", |log| {
        has_line(log, "1: probe at load ready")
    });
    assert!(
        !log.contains("change global.probe ready"),
        "the join must not run change handlers: {log}"
    );

    s.h.fx
        .manager()
        .call_export(RESOURCE, "poke", &[], None)
        .expect("poke is exported");
    s.h.wait_for_report(&s.runtime, "the state change", |log| {
        has_line(
            log,
            "1: change global.probe changed readable changed replicated true",
        )
    });
}

#[test]
fn client_replicated_writes_are_refused_and_local_writes_stay_on_the_client() {
    let s = session();
    s.h.wait_for_report(&s.runtime, "the script's writes", |log| {
        has_line(log, "1: replicated write refused true") && has_line(log, "1: local yes")
    });

    let refused = s
        .runtime
        .state_set(GLOBAL_BAG, "probe", Value::Int(1), true);
    assert!(refused.is_err(), "a replicated write must be refused");

    s.runtime
        .state_set(GLOBAL_BAG, "local_only", Value::Int(5), false)
        .expect("a local write works");
    assert_eq!(
        s.runtime.state_get(GLOBAL_BAG, "local_only"),
        Some(Value::Int(5))
    );
    assert_eq!(s.h.fx.manager().state_get(GLOBAL_BAG, "local_only"), None);
}

#[test]
fn console_runs_the_script_commands_of_the_client() {
    let s = session();
    s.runtime.execute("probe yes");
    s.h.wait_for_report(&s.runtime, "the command's report", |log| {
        has_line(log, "1: command yes")
    });
}

#[test]
fn is_duplicity_version_is_false_on_the_client() {
    let s = session();
    s.h.wait_for_report(&s.runtime, "the duplicity check", |log| {
        has_line(log, "1: duplicity false")
    });
}

#[test]
fn client_exports_declared_by_one_resource_are_callable_from_another() {
    let h = Harness::bare("consumer");
    h.add(
        "provider",
        "fx_version 'cerulean'\ngame 'common'\nexport 'greet'\nclient_scripts { 'client.lua' }\n",
        &[("client.lua", PROVIDER_CLIENT_SCRIPT)],
    );
    let consumer =
        manifest(&["server.lua"], &["report"], &["provider"]) + "client_scripts { 'client.lua' }\n";
    h.add(
        "consumer",
        &consumer,
        &[
            ("server.lua", CONSUMER_SERVER_SCRIPT),
            ("client.lua", CONSUMER_CLIENT_SCRIPT),
        ],
    );
    h.start("consumer");

    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let log = h.wait_for_report(&runtime, "the export calls", |log| {
        has_line(log, "1: greet true hello bob") && has_line(log, "1: undeclared false")
    });
    assert!(has_line(&log, "1: greet true hello bob"), "{log}");
    assert!(has_line(&log, "1: undeclared false"), "{log}");
}

#[test]
fn restart_on_the_server_stops_and_restarts_the_client_runtime() {
    let s = session();
    s.h.wait_for_report(&s.runtime, "the first start and its pong", |log| {
        has_line(log, "1: started") && has_line(log, "1: pong 65535 pong for from the client")
    });

    // A restart replaces the server's runtime, and with it the report log, so
    // the log only holds what the stopped client and the new one reported.
    s.h.fx.server.execute(&format!("restart {RESOURCE}"));
    s.h.wait_for_report(&s.runtime, "the stop and the second start", |log| {
        has_line(log, "1: stopped") && has_line(log, "1: started")
    });
}

#[test]
fn client_reads_declared_files_and_writes_only_its_own_data_folder() {
    let h = Harness::bare("files_demo");
    let text = manifest(&["server.lua"], &["report", "poke"], &[])
        + "client_scripts { 'client.lua' }\nfiles { 'data/config.json' }\n";
    h.add(
        "files_demo",
        &text,
        &[
            ("server.lua", SERVER_SCRIPT),
            ("client.lua", FILES_CLIENT_SCRIPT),
            ("data/config.json", r#"{"answer":42}"#),
        ],
    );
    h.start("files_demo");
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let log = h.wait_for_report(&runtime, "the file reports", |log| {
        has_line(log, "1: missing nil")
    });
    assert!(has_line(&log, r#"1: config {"answer":42}"#), "{log}");
    assert!(has_line(&log, "1: save declared false"), "{log}");
    assert!(has_line(&log, "1: save own true"), "{log}");
    assert!(has_line(&log, "1: load own mine"), "{log}");
}

#[test]
fn client_resource_natives_report_state_metadata_and_resources() {
    let h = Harness::bare("hello");
    let text = manifest(&["server.lua"], &["report", "poke"], &[])
        + "version '1.2.0'\nclient_scripts { 'client.lua' }\n";
    h.add(
        "hello",
        &text,
        &[
            ("server.lua", SERVER_SCRIPT),
            ("client.lua", RESOURCE_INFO_CLIENT_SCRIPT),
        ],
    );
    h.start("hello");
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let log = h.wait_for_report(&runtime, "the resource natives", |log| {
        has_line(log, "1: state started")
    });
    assert!(has_line(&log, "1: state starting"), "{log}");
    assert!(has_line(&log, "1: version 1.2.0"), "{log}");
    assert!(has_line(&log, "1: count 1 hello"), "{log}");
}

/// Server side of the command tests: a restricted and an unrestricted command,
/// a probe of `IsPlayerAceAllowed`, and the license of each joining player.
const COMMANDS_SCRIPT: &str = r"
local lines = {}

local function record(line)
    lines[#lines + 1] = line
end

AddEventHandler('playerJoining', function()
    record(('license %s'):format(GetPlayerIdentifierByType(source, 'license') or '-'))
end)

RegisterCommand('hello', function(source, args)
    record(('hello %d %s'):format(source, args[1] or '-'))
end, false)

RegisterCommand('secret', function(source)
    record(('secret %d'):format(source))
end, true)

RegisterCommand('probe', function(source, args)
    record(('probe %d %s %s'):format(source, args[1], tostring(IsPlayerAceAllowed(source, args[1]))))
end, false)

function report()
    return table.concat(lines, '\n')
end
";

/// Name of the resource that holds [`COMMANDS_SCRIPT`].
const COMMANDS_RESOURCE: &str = "commands";

/// A server with the command resource running and a client that has joined.
struct CommandSession {
    h: Harness,
    runtime: Rc<ClientRuntime>,
    /// The player's license identifier, `license:<hex>`.
    license: String,
    _dir: TempDir,
}

fn command_session() -> CommandSession {
    let h = Harness::bare(COMMANDS_RESOURCE);
    h.add(
        COMMANDS_RESOURCE,
        &manifest(&["server.lua"], &["report"], &[]),
        &[("server.lua", COMMANDS_SCRIPT)],
    );
    h.start(COMMANDS_RESOURCE);
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let log = h.wait_for_report(&runtime, "the join", |log| has_prefix(log, "license "));
    let license = log
        .lines()
        .find_map(|line| line.strip_prefix("license "))
        .expect("the join is recorded")
        .to_owned();
    CommandSession {
        h,
        runtime,
        license,
        _dir: dir,
    }
}

/// Whether some line of the report log starts with `prefix`.
fn has_prefix(log: &str, prefix: &str) -> bool {
    log.lines().any(|line| line.starts_with(prefix))
}

/// Runs the console line `line` on the server, as the maintainer would.
fn console(h: &Harness, line: &str) {
    h.fx.server.execute(line);
}

#[test]
fn player_commands_run_with_the_player_id_and_restricted_ones_need_an_entry() {
    let s = command_session();
    s.runtime.execute("hello world");
    s.h.wait_for_report(&s.runtime, "the hello", |log| {
        has_line(log, "hello 1 world")
    });

    s.runtime.execute("secret");
    s.runtime.execute("hello refusal");
    let log = s.h.wait_for_report(&s.runtime, "the second hello", |log| {
        has_line(log, "hello 1 refusal")
    });
    assert!(
        !has_prefix(&log, "secret"),
        "a restricted command ran:\n{log}"
    );

    console(
        &s.h,
        &format!("add_ace identifier.{} command.secret allow", s.license),
    );
    s.runtime.execute("secret");
    s.h.wait_for_report(&s.runtime, "the allowed secret", |log| {
        has_line(log, "secret 1")
    });

    s.runtime.execute("probe command.secret");
    s.runtime.execute("probe command.quit");
    let log = s.h.wait_for_report(&s.runtime, "the probes", |log| {
        has_line(log, "probe 1 command.quit false")
    });
    assert!(has_line(&log, "probe 1 command.secret true"), "{log}");
}

#[test]
fn group_membership_grants_commands_through_the_player_principal() {
    let s = command_session();
    s.runtime.execute("probe command.quit");
    s.h.wait_for_report(&s.runtime, "the first probe", |log| {
        has_line(log, "probe 1 command.quit false")
    });

    console(
        &s.h,
        &format!("add_principal identifier.{} group.admin", s.license),
    );
    console(&s.h, "add_ace group.admin command allow");
    s.runtime.execute("probe command.quit");
    s.h.wait_for_report(&s.runtime, "the group probe", |log| {
        has_line(log, "probe 1 command.quit true")
    });
}

#[test]
fn players_cannot_quit_stop_or_read_and_set_convars_through_the_server() {
    let h = Harness::bare(COMMANDS_RESOURCE);
    h.add(
        COMMANDS_RESOURCE,
        &manifest(&["server.lua"], &["report"], &[]),
        &[("server.lua", COMMANDS_SCRIPT)],
    );
    h.start(COMMANDS_RESOURCE);
    let dir = tempfile::tempdir().expect("temp dir");
    let client = h.connect_client(&dir.path().join("cache"));
    let before = h.fx.manager().convar("sv_hostname");

    for line in [
        "quit",
        "stop commands",
        "sv_hostname hacked",
        "hello refused",
    ] {
        client.send_command(line).expect("the command is sent");
    }
    h.wait_for_server_log("the last hello", |log| has_line(log, "hello 1 refused"));

    assert!(
        !h.fx.manager().quit_requested(),
        "a player stopped the server"
    );
    assert!(h.fx.manager().is_running(COMMANDS_RESOURCE));
    assert_eq!(h.fx.manager().convar("sv_hostname"), before);
    assert_ne!(
        h.fx.manager().convar("sv_hostname").as_deref(),
        Some("hacked")
    );
}

/// Name of the resource that holds [`CONVARS_SERVER_SCRIPT`] and [`CONVARS_CLIENT_SCRIPT`].
const CONVARS_RESOURCE: &str = "convars";

/// Server side of the convar tests: records the client's reports, replicates a
/// convar from a command, and reads booleans as a server script.
const CONVARS_SERVER_SCRIPT: &str = r"
local lines = {}

RegisterNetEvent('convars:report', function(text)
    lines[#lines + 1] = text
end)

RegisterCommand('replicate', function(_, args)
    SetConvarReplicated(args[1], args[2])
end, false)

RegisterCommand('boolprobe', function(_, args)
    lines[#lines + 1] = ('server %s %s'):format(args[1], tostring(GetConvarBool(args[1], true)))
end, false)

function report()
    return table.concat(lines, '\n')
end
";

/// Client side of the convar tests: `show` reports `game_mode`, and `boolprobe`
/// reports a boolean convar.
const CONVARS_CLIENT_SCRIPT: &str = r"
local function report(text)
    TriggerServerEvent('convars:report', text)
end

-- Runs while the script loads, so it sees only what the join delivered first.
report('load ' .. GetConvar('game_mode', 'none'))

RegisterCommand('show', function()
    report('show ' .. GetConvar('game_mode', 'none'))
end, false)

RegisterCommand('boolprobe', function(_, args)
    report(('client %s %s'):format(args[1], tostring(GetConvarBool(args[1], true))))
end, false)
";

/// A server with the convar resource running and no client yet.
fn convars_harness() -> Harness {
    let h = Harness::bare(CONVARS_RESOURCE);
    let text = manifest(&["server.lua"], &["report"], &[]) + "client_scripts { 'client.lua' }\n";
    h.add(
        CONVARS_RESOURCE,
        &text,
        &[
            ("server.lua", CONVARS_SERVER_SCRIPT),
            ("client.lua", CONVARS_CLIENT_SCRIPT),
        ],
    );
    h.start(CONVARS_RESOURCE);
    h
}

impl Harness {
    /// Runs both loops for `span`, handling every server input and client notice
    /// that arrives in that time.
    fn settle(&self, runtime: &ClientRuntime, span: Duration) {
        let end = Instant::now() + span;
        while Instant::now() < end {
            match self.input.recv_timeout(STEP) {
                Ok(input) => self.fx.server.handle(input),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
            self.pump_server();
            runtime.pump(STEP);
        }
    }
}

/// Runs the client command `command` until the server's log ends with the line
/// `expected`, and returns the log. Each attempt adds a report, so the last line
/// is the client's latest answer.
fn client_reports_last(
    h: &Harness,
    runtime: &ClientRuntime,
    command: &str,
    expected: &str,
) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        runtime.execute(command);
        h.settle(runtime, CONVARS_SETTLE);
        let log = h.fx.text(h.reporter, "report");
        if log.lines().last() == Some(expected) {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for `{expected}` as the last line; the server has:\n{log}"
        );
    }
}

/// How long a convar test lets both loops run after each client command.
const CONVARS_SETTLE: Duration = Duration::from_millis(100);

#[test]
fn a_replicated_convar_set_before_the_join_is_read_by_the_client_script() {
    let h = convars_harness();
    console(&h, "setr game_mode race");
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    client_reports_last(&h, &runtime, "show", "show race");
}

#[test]
fn client_scripts_see_replicated_convars_while_they_load() {
    let h = convars_harness();
    console(&h, "setr game_mode race");
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let log = h.wait_for_report(&runtime, "the load report", |log| {
        log.lines().any(|line| line.starts_with("load "))
    });
    assert!(has_line(&log, "load race"), "{log}");
}

#[test]
fn later_changes_reach_the_client_and_a_store_of_another_kind_removes_it() {
    let h = convars_harness();
    console(&h, "setr game_mode race");
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    client_reports_last(&h, &runtime, "show", "show race");

    console(&h, "setr game_mode drift");
    client_reports_last(&h, &runtime, "show", "show drift");

    // Server information is marked, not sent, so the client forgets the convar.
    console(&h, "sets game_mode info");
    assert_eq!(
        h.fx.manager().convar_kind("game_mode"),
        Some(ConvarKind::ServerInfo)
    );
    client_reports_last(&h, &runtime, "show", "show none");

    console(&h, "setr game_mode again");
    client_reports_last(&h, &runtime, "show", "show again");

    // A plain set is the last store, so the replication ends.
    console(&h, "set game_mode x");
    assert_eq!(
        h.fx.manager().convar_kind("game_mode"),
        Some(ConvarKind::Plain)
    );
    client_reports_last(&h, &runtime, "show", "show none");
}

#[test]
fn a_local_set_of_a_replicated_convar_is_refused_and_the_value_stays() {
    let h = convars_harness();
    console(&h, "setr game_mode race");
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    client_reports_last(&h, &runtime, "show", "show race");

    runtime.execute("set game_mode y");
    runtime.execute("setr game_mode y");
    runtime.execute("game_mode y");
    client_reports_last(&h, &runtime, "show", "show race");
}

#[test]
fn a_script_on_the_server_replicates_a_convar_to_the_client() {
    let h = convars_harness();
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    client_reports_last(&h, &runtime, "show", "show none");

    console(&h, "replicate game_mode gamma");
    assert_eq!(
        h.fx.manager().convar_kind("game_mode"),
        Some(ConvarKind::Replicated)
    );
    client_reports_last(&h, &runtime, "show", "show gamma");
}

#[test]
fn get_convar_bool_reads_the_usual_spellings_on_both_sides() {
    let h = convars_harness();
    console(&h, "setr flag_yes YES");
    console(&h, "set flag_no OFF");
    console(&h, "boolprobe flag_yes");
    console(&h, "boolprobe flag_no");
    console(&h, "boolprobe flag_missing");
    let server_log = h.fx.text(CONVARS_RESOURCE, "report");
    assert!(
        has_line(&server_log, "server flag_yes true"),
        "{server_log}"
    );
    assert!(
        has_line(&server_log, "server flag_no false"),
        "{server_log}"
    );
    assert!(
        has_line(&server_log, "server flag_missing true"),
        "{server_log}"
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    runtime.execute("set flag_off off");
    runtime.execute("boolprobe flag_yes");
    runtime.execute("boolprobe flag_off");
    let log = h.wait_for_report(&runtime, "the client bool probes", |log| {
        has_line(log, "client flag_off false")
    });
    assert!(has_line(&log, "client flag_yes true"), "{log}");
}
