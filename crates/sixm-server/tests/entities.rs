//! Entities end to end: a server resource creates, moves and deletes a vehicle,
//! and a `ClientRuntime` mirrors it.
//!
//! As in `client_scripts.rs`, the test thread plays both loops. Server inputs
//! are handled and frames are ticked between client pumps, and the client's
//! reports come back to the server as events, where the server resource keeps
//! them for the test to read.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use common::{Fixture, manifest};
use sixm_client::{Client, ClientRuntime};
use sixm_core::{Host, Value};
use sixm_net::Identity;
use sixm_server::Input;

/// Longest a test waits for the server or the client to do something.
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long each wait step blocks before checking again.
const STEP: Duration = Duration::from_millis(5);
/// The resource the tests start.
const RESOURCE: &str = "entities_demo";

/// Server side: creates the vehicle on `spawn`, moves it twice in one handler on
/// `entities:move`, deletes it on `entities:remove`, and records the events.
const SERVER_SCRIPT: &str = r"
local lines = {}
local vehicle

local function record(line)
    lines[#lines + 1] = line
end

AddEventHandler('entityCreated', function(id)
    record(('created %d'):format(id))
end)

AddEventHandler('entityRemoved', function(id)
    record(('removed %d'):format(id))
end)

AddEventHandler('playerJoining', function()
    if vehicle then
        TriggerClientEvent('entities:id', source, vehicle)
    end
end)

RegisterNetEvent('entities:report', function(text)
    record(('%d: %s'):format(source, text))
end)

RegisterNetEvent('entities:move', function()
    SetEntityCoords(vehicle, 10.0, 20.0, 30.0)
    SetEntityCoords(vehicle, 4.0, 5.0, 6.0)
end)

RegisterNetEvent('entities:remove', function()
    DeleteEntity(vehicle)
    vehicle = nil
end)

function spawn()
    vehicle = CreateVehicle('adder', 1.0, 2.0, 3.0, 90.0, true, false)
    Entity(vehicle).state.plate = 'SIXM'
    TriggerClientEvent('entities:id', -1, vehicle)
    return vehicle
end

function report()
    return table.concat(lines, '\n')
end
";

/// Client side: once it learns the vehicle's id, it reports what it sees, moves
/// the vehicle, waits for the new position and then asks for its removal.
const CLIENT_SCRIPT: &str = r"
local function report(text)
    TriggerServerEvent('entities:report', text)
end

RegisterNetEvent('entities:id', function(id)
    CreateThread(function()
        report(('exists %s'):format(tostring(DoesEntityExist(id))))
        local c = GetEntityCoords(id)
        report(('coords %.1f %.1f %.1f'):format(c.x, c.y, c.z))
        report(('heading %.1f'):format(GetEntityHeading(id)))
        report(('model %s'):format(tostring(GetEntityModel(id) == GetHashKey('adder'))))
        report(('plate %s'):format(tostring(Entity(id).state.plate)))

        TriggerServerEvent('entities:move')
        while DoesEntityExist(id) and GetEntityCoords(id).x ~= 4.0 do
            Wait(0)
        end
        c = GetEntityCoords(id)
        report(('moved %.1f %.1f %.1f'):format(c.x, c.y, c.z))

        TriggerServerEvent('entities:remove')
        while DoesEntityExist(id) do
            Wait(0)
        end
        report('gone')
    end)
end)
";

/// A server listening on loopback, whose inputs are handled by the test thread.
struct Harness {
    fx: Fixture,
    input: Receiver<Input>,
    addr: SocketAddr,
}

impl Harness {
    /// A server with the entity resource started and its network running.
    fn new() -> Self {
        let fx = Fixture::new();
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        let text = manifest(&["server.lua"], &["spawn", "report"], &[])
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

    /// Creates the vehicle through the resource and returns its id.
    fn spawn(&self) -> u32 {
        let values = self
            .fx
            .manager()
            .call_export(RESOURCE, "spawn", &[])
            .expect("spawn is exported");
        match values.first() {
            Some(Value::Int(id)) => u32::try_from(*id).expect("ids fit in u32"),
            other => panic!("spawn returned {other:?}, not an id"),
        }
    }

    /// Connects a client that keeps its files in `cache` and its data in
    /// `data`, then starts the resources it synced.
    fn connect(&self, cache: &Path, data: &Path) -> Rc<ClientRuntime> {
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
        let client = handle
            .join()
            .expect("the client thread does not panic")
            .expect("the client connects and syncs");
        let runtime = ClientRuntime::new(client, data);
        runtime.start_synced();
        runtime
    }

    /// Runs both loops, ticking the server frames, until the server's report
    /// log satisfies `done`, and returns the log.
    fn wait_for_report(
        &self,
        runtime: &ClientRuntime,
        what: &str,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump_server();
            self.fx.server.tick(Instant::now());
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

/// Whether the report log has a line exactly `line`.
fn has_line(log: &str, line: &str) -> bool {
    log.lines().any(|candidate| candidate == line)
}

#[test]
fn a_connected_client_follows_creation_moves_and_deletion() {
    let h = Harness::new();
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let id = h.spawn();

    let log = h.wait_for_report(&runtime, "the vehicle to be deleted", |log| {
        has_line(log, "1: gone") && has_line(log, &format!("removed {id}"))
    });

    assert!(has_line(&log, &format!("created {id}")), "{log}");
    assert!(has_line(&log, "1: exists true"), "{log}");
    assert!(has_line(&log, "1: coords 1.0 2.0 3.0"), "{log}");
    assert!(has_line(&log, "1: heading 90.0"), "{log}");
    assert!(has_line(&log, "1: model true"), "{log}");
    assert!(has_line(&log, "1: plate SIXM"), "{log}");
    assert!(has_line(&log, "1: moved 4.0 5.0 6.0"), "{log}");
}

#[test]
fn a_client_that_joins_later_sees_the_entity_in_its_snapshot() {
    let h = Harness::new();
    let id = h.spawn();
    let dir = tempfile::tempdir().expect("temp dir");
    let runtime = h.connect(&dir.path().join("cache"), &dir.path().join("data"));

    let log = h.wait_for_report(&runtime, "the joining client's report", |log| {
        has_line(log, "1: plate SIXM")
    });

    assert!(has_line(&log, &format!("created {id}")), "{log}");
    assert!(has_line(&log, "1: exists true"), "{log}");
    assert!(has_line(&log, "1: coords 1.0 2.0 3.0"), "{log}");
    assert!(has_line(&log, "1: model true"), "{log}");
}
