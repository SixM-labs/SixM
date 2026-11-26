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
use sixm_core::{GLOBAL_BAG, Host, Value};
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
        let addr = self.addr;
        let cache = cache.to_path_buf();
        let handle = thread::spawn(move || Client::connect(addr, "tester", None, cache, TIMEOUT));
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
        let client = handle
            .join()
            .expect("the client thread does not panic")
            .expect("the client connects and syncs");
        let runtime = ClientRuntime::new(client, data);
        runtime.start_synced();
        runtime
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
    s.h.wait_for_report(&s.runtime, "the initial state", |log| {
        has_line(
            log,
            "1: change global.probe ready readable ready replicated true",
        )
    });

    s.h.fx
        .manager()
        .call_export(RESOURCE, "poke", &[])
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
