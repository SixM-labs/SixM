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

/// A server listening on loopback, whose inputs are handled by the test thread.
struct Harness {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
}

impl Harness {
    /// A server with the `hello` resource started and its network running.
    fn new() -> Self {
        let fx = Fixture::new();
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        let text = manifest(&["server.lua"], &["report", "poke"], &[])
            + "client_scripts { 'client.lua' }\n";
        fx.add_resource(
            RESOURCE,
            &text,
            &[("server.lua", SERVER_SCRIPT), ("client.lua", CLIENT_SCRIPT)],
        );
        fx.server.execute(&format!("start {RESOURCE}"));
        Self { fx, input, addr }
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
            let log = self.fx.text(RESOURCE, "report");
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
