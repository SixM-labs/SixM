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
use sixm_client::{Client, ClientNotice, ClientRuntime, Input as ClientInput};
use sixm_core::{EntityKind, Host, Value};
use sixm_net::Identity;
use sixm_server::Input;

/// Longest a test waits for the server or the client to do something.
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long each wait step blocks before checking again.
const STEP: Duration = Duration::from_millis(5);
/// The resource the vehicle tests start.
const RESOURCE: &str = "entities_demo";
/// Time between two console lines a test types into a client.
const ASK_INTERVAL: Duration = Duration::from_millis(200);

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
    /// The resource the server runs, which holds the report log.
    resource: &'static str,
}

impl Harness {
    /// A server with the vehicle resource started and its network running.
    fn new() -> Self {
        Self::start(RESOURCE, SERVER_SCRIPT, CLIENT_SCRIPT, &["spawn", "report"])
    }

    /// A server with `resource` started, whose server and client scripts are
    /// `server` and `client`, and which exports `exports` from the server.
    fn start(resource: &'static str, server: &str, client: &str, exports: &[&str]) -> Self {
        let fx = Fixture::new();
        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx
            .server
            .start_network(sender)
            .expect("the network starts on loopback");
        let text = manifest(&["server.lua"], exports, &[]) + "client_scripts { 'client.lua' }\n";
        fx.add_resource(
            resource,
            &text,
            &[("server.lua", server), ("client.lua", client)],
        );
        fx.server.execute(&format!("start {resource}"));
        Self {
            fx,
            input,
            addr,
            resource,
        }
    }

    /// Handles every queued server input.
    fn pump_server(&self) {
        while let Ok(input) = self.input.try_recv() {
            self.fx.server.handle(input);
        }
    }

    /// Creates the vehicle through the resource and returns its id.
    fn spawn(&self) -> u32 {
        self.number("spawn", &[])
    }

    /// Calls export `name` with `args` and returns its first result, which must
    /// be a whole number.
    fn number(&self, name: &str, args: &[Value]) -> u32 {
        match self.call(name, args) {
            Value::Int(id) => u32::try_from(id).expect("a number that fits in u32"),
            other => panic!("export `{name}` returned {other:?}, not a number"),
        }
    }

    /// Calls export `name` with `args` and returns its first result.
    fn call(&self, name: &str, args: &[Value]) -> Value {
        self.fx
            .manager()
            .call_export(self.resource, name, args, None)
            .unwrap_or_else(|err| panic!("export `{name}`: {err}"))
            .into_iter()
            .next()
            .unwrap_or(Value::Nil)
    }

    /// The report log of the server resource.
    fn log(&self) -> String {
        self.fx.text(self.resource, "report")
    }

    /// Connects a client that keeps its files in `cache` and its data in
    /// `data`, then starts the resources it synced.
    fn connect(&self, cache: &Path, data: &Path) -> Rc<ClientRuntime> {
        let runtime = ClientRuntime::new(self.connect_client(cache), data);
        runtime.start_synced();
        runtime
    }

    /// Connects a client that keeps its files in `cache`, and returns the
    /// connection itself, so that a test can read its notices.
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
            let log = self.log();
            if done(&log) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the server has:\n{log}"
            );
        }
    }

    /// Like [`wait_for_report`](Self::wait_for_report), but types the console
    /// line `line` into `asked` every [`ASK_INTERVAL`], so that it reports again
    /// with what it sees now. Every client in `runtimes` is run, `asked` among them.
    fn ask_until(
        &self,
        asked: &ClientRuntime,
        runtimes: &[&ClientRuntime],
        line: &str,
        what: &str,
        done: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + TIMEOUT;
        let mut last: Option<Instant> = None;
        loop {
            self.pump_server();
            self.fx.server.tick(Instant::now());
            for runtime in runtimes {
                runtime.pump(Duration::ZERO);
                runtime.tick(Instant::now());
            }
            if last.is_none_or(|at| at.elapsed() >= ASK_INTERVAL) {
                asked.handle(ClientInput::Line(line.to_owned()));
                last = Some(Instant::now());
            }
            thread::sleep(STEP);
            let log = self.log();
            if done(&log) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the server has:\n{log}"
            );
        }
    }

    /// Runs both loops for `frames` frames, so that anything sent has arrived.
    fn run_frames(&self, runtimes: &[&ClientRuntime], frames: usize) {
        for _ in 0..frames {
            self.pump_server();
            self.fx.server.tick(Instant::now());
            for runtime in runtimes {
                runtime.pump(Duration::ZERO);
                runtime.tick(Instant::now());
            }
            thread::sleep(STEP);
        }
    }

    /// Runs both loops until `done` holds, without any report to wait for.
    fn settle_until(&self, runtimes: &[&ClientRuntime], what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done() {
            self.pump_server();
            self.fx.server.tick(Instant::now());
            for runtime in runtimes {
                runtime.pump(Duration::ZERO);
                runtime.tick(Instant::now());
            }
            thread::sleep(STEP);
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
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

/// The resource of the ped tests.
const PEDS_RESOURCE: &str = "peds_demo";

/// Server side of the ped tests: reads and moves the peds of the players, and
/// reports what it sees. Each report line starts with the player's id.
const PEDS_SERVER: &str = r"
local lines = {}

local function record(line)
    lines[#lines + 1] = line
end

RegisterNetEvent('peds:report', function(text)
    record(('%d: %s'):format(source, text))
end)

function report()
    return table.concat(lines, '\n')
end

function ped_of(player)
    return GetPlayerPed(player)
end

function coords_of(player)
    local c = GetEntityCoords(GetPlayerPed(player))
    return ('%.1f %.1f %.1f'):format(c.x, c.y, c.z)
end

function teleport()
    SetEntityCoords(GetPlayerPed(1), 100.0, 0.0, 0.0)
end

function vehicle()
    return CreateVehicle('adder', 0.0, 0.0, 0.0, 0.0, true, false)
end

function owner_of(entity)
    return NetworkGetEntityOwner(entity)
end

function exists(entity)
    return DoesEntityExist(entity)
end

function model_is_player(entity)
    return GetEntityModel(entity) == GetHashKey('player')
end
";

/// Client side of the ped tests. Commands run from the test, and their reports
/// go back to the server.
const PEDS_CLIENT: &str = r"
local function report(text)
    TriggerServerEvent('peds:report', text)
end

-- Every ped of the pool except the local player's own.
local function others()
    local own = PlayerPedId()
    local found = {}
    for _, ped in ipairs(GetGamePool('CPed')) do
        if ped ~= own then
            found[#found + 1] = ped
        end
    end
    return found
end

RegisterCommand('moveme', function()
    SetEntityCoords(PlayerPedId(), 5.0, 6.0, 7.0)
end, false)

-- Tries to move every other ped, which must do nothing.
RegisterCommand('steal', function()
    for _, ped in ipairs(others()) do
        SetEntityCoords(ped, 0.0, 0.0, 0.0)
    end
end, false)

RegisterCommand('look', function()
    local own = PlayerPedId()
    local c = GetEntityCoords(own)
    report(('own %d %.1f %.1f %.1f'):format(own, c.x, c.y, c.z))
    for _, ped in ipairs(others()) do
        local o = GetEntityCoords(ped)
        report(('other %d %.1f %.1f %.1f'):format(ped, o.x, o.y, o.z))
    end
end, false)

RegisterCommand('watch', function()
    CreateThread(function()
        local own = PlayerPedId()
        while GetEntityCoords(own).x ~= 100.0 do
            Wait(0)
        end
        local c = GetEntityCoords(own)
        report(('teleported %.1f %.1f %.1f'):format(c.x, c.y, c.z))
    end)
end, false)
";

/// The report line a player's client sent, which starts with the player's id.
fn reported(player: u32, text: &str) -> String {
    format!("{player}: {text}")
}

#[test]
fn players_move_their_own_peds_and_everyone_sees_them() {
    let h = Harness::start(
        PEDS_RESOURCE,
        PEDS_SERVER,
        PEDS_CLIENT,
        &[
            "report",
            "ped_of",
            "coords_of",
            "teleport",
            "vehicle",
            "owner_of",
            "exists",
            "model_is_player",
        ],
    );
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));

    // Player 1 is `a`, and its ped exists on the server from the moment it joins.
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    let ped_b = h.number("ped_of", &[Value::Int(2)]);
    assert_ne!(ped_a, 0);
    assert_ne!(ped_b, 0);
    // The default ped model is `player`, from `sixm_playerModel`.
    assert_eq!(
        h.call("model_is_player", &[Value::Int(i64::from(ped_a))]),
        Value::Bool(true)
    );
    assert_eq!(h.number("owner_of", &[Value::Int(i64::from(ped_a))]), 1);
    assert_eq!(h.number("owner_of", &[Value::Int(i64::from(ped_b))]), 2);
    let vehicle = h.number("vehicle", &[]);
    assert_eq!(
        h.call("owner_of", &[Value::Int(i64::from(vehicle))]),
        Value::Int(-1),
        "a script-created entity has no owner"
    );

    // Each client knows its own ped and sees the other one at the origin.
    h.ask_until(&a, &[&a, &b], "look", "a to see its own ped", |log| {
        has_line(log, &reported(1, &format!("own {ped_a} 0.0 0.0 0.0")))
    });
    h.ask_until(&b, &[&a, &b], "look", "b to see a's ped", |log| {
        has_line(log, &reported(2, &format!("other {ped_a} 0.0 0.0 0.0")))
    });

    // `a` moves its ped, and `b` sees the new position.
    a.handle(ClientInput::Line("moveme".to_owned()));
    h.ask_until(&b, &[&a, &b], "look", "b to see a's ped move", |log| {
        has_line(log, &reported(2, &format!("other {ped_a} 5.0 6.0 7.0")))
    });
    assert_eq!(
        h.call("coords_of", &[Value::Int(1)]),
        Value::from("5.0 6.0 7.0")
    );

    // `b` cannot move the ped of `a`, on its client or on the server.
    b.handle(ClientInput::Line("steal".to_owned()));
    h.run_frames(&[&a, &b], 40);
    assert_eq!(
        h.call("coords_of", &[Value::Int(1)]),
        Value::from("5.0 6.0 7.0")
    );
    assert_eq!(
        b.entity(ped_a).map(|info| info.position),
        Some([5.0, 6.0, 7.0])
    );

    // The server moves `a`'s ped, and `a` takes the teleport although it owns the ped.
    h.call("teleport", &[]);
    a.handle(ClientInput::Line("watch".to_owned()));
    h.wait_for_report(&a, "a to see its ped teleported", |log| {
        has_line(log, &reported(1, "teleported 100.0 0.0 0.0"))
    });
    assert_eq!(
        a.entity(ped_a).map(|info| info.position),
        Some([100.0, 0.0, 0.0])
    );
    h.ask_until(&b, &[&a, &b], "look", "b to see the teleport", |log| {
        has_line(log, &reported(2, &format!("other {ped_a} 100.0 0.0 0.0")))
    });

    // When `a` leaves, its ped is deleted everywhere.
    drop(a);
    h.settle_until(&[&b], "a's ped to be deleted", || {
        h.call("exists", &[Value::Int(i64::from(ped_a))]) == Value::Bool(false)
            && b.entities(Some(EntityKind::Ped)) == [ped_b]
    });
    assert_eq!(h.call("ped_of", &[Value::Int(1)]), Value::Int(0));
}

#[test]
fn a_move_sent_before_a_teleport_does_not_undo_the_teleport() {
    let h = Harness::start(
        PEDS_RESOURCE,
        PEDS_SERVER,
        PEDS_CLIENT,
        &[
            "report",
            "ped_of",
            "coords_of",
            "teleport",
            "owner_of",
            "exists",
        ],
    );
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    h.ask_until(&a, &[&a, &b], "look", "a to see its own ped", |log| {
        has_line(log, &reported(1, &format!("own {ped_a} 0.0 0.0 0.0")))
    });

    // `a` moves its ped, but its update is still on its side when the server
    // teleports the ped, so the update was computed before the teleport.
    a.handle(ClientInput::Line("moveme".to_owned()));
    h.call("teleport", &[]);
    h.fx.server.tick(Instant::now());
    a.tick(Instant::now());

    // The stale update reaches the server after the teleport and is refused,
    // and `a` then takes the teleport from the server.
    h.run_frames(&[&a, &b], 60);
    assert_eq!(
        a.entity(ped_a).map(|info| info.position),
        Some([100.0, 0.0, 0.0]),
        "a takes the teleport"
    );
    assert_eq!(
        h.call("coords_of", &[Value::Int(1)]),
        Value::from("100.0 0.0 0.0")
    );

    // `a` has stopped moving. Everyone keeps the teleport position.
    h.ask_until(&a, &[&a, &b], "look", "a to report the teleport", |log| {
        has_line(log, &reported(1, &format!("own {ped_a} 100.0 0.0 0.0")))
    });
    h.ask_until(&b, &[&a, &b], "look", "b to report the teleport", |log| {
        has_line(log, &reported(2, &format!("other {ped_a} 100.0 0.0 0.0")))
    });
    h.run_frames(&[&a, &b], 60);
    assert_eq!(
        b.entity(ped_a).map(|info| info.position),
        Some([100.0, 0.0, 0.0]),
        "b keeps the teleport position"
    );
    assert_eq!(
        h.call("coords_of", &[Value::Int(1)]),
        Value::from("100.0 0.0 0.0")
    );
    assert_eq!(h.number("owner_of", &[Value::Int(i64::from(ped_a))]), 1);
    assert_eq!(
        h.call("exists", &[Value::Int(i64::from(ped_a))]),
        Value::Bool(true)
    );
}

/// The resource of the scoping tests.
const SCOPE_RESOURCE: &str = "scope_demo";

/// The exports of the scoping tests, from the server script.
const SCOPE_EXPORTS: &[&str] = &[
    "report",
    "view",
    "ped_of",
    "spawn_vehicle",
    "paint",
    "set_bucket",
    "set_vehicle_bucket",
];

/// Server side of the scoping tests. Keeps the latest view each player reported,
/// and a vehicle that every player is told about.
const SCOPE_SERVER: &str = r"
local views = {}
local vehicle

RegisterNetEvent('scope:view', function(text)
    views[source] = text
end)

AddEventHandler('playerJoining', function()
    if vehicle then
        TriggerClientEvent('scope:vehicle', source, vehicle)
    end
end)

function view(player)
    return views[player] or ''
end

function report()
    local lines = {}
    for player, text in pairs(views) do
        lines[#lines + 1] = ('%d: %s'):format(player, text)
    end
    table.sort(lines)
    return table.concat(lines, '\n')
end

function ped_of(player)
    return GetPlayerPed(player)
end

function spawn_vehicle(x, y, z)
    vehicle = CreateVehicle('adder', x, y, z, 0.0, true, false)
    TriggerClientEvent('scope:vehicle', -1, vehicle)
    return vehicle
end

function paint(color)
    Entity(vehicle).state.color = color
end

function set_bucket(player, bucket)
    SetPlayerRoutingBucket(player, bucket)
end

function set_vehicle_bucket(bucket)
    SetEntityRoutingBucket(vehicle, bucket)
end
";

/// Client side of the scoping tests. `far` and `home` move the local ped, and
/// `look` reports the peds in the client's pool, and the vehicle the server
/// spawned: whether it exists here and the colour its state bag holds. Each
/// report starts with a look count, so a test can tell a fresh report from an
/// old one.
const SCOPE_CLIENT: &str = r"
local looks = 0
local vehicle_id

RegisterNetEvent('scope:vehicle', function(id)
    vehicle_id = id
end)

RegisterCommand('far', function()
    SetEntityCoords(PlayerPedId(), 1000.0, 0.0, 0.0)
end, false)

RegisterCommand('home', function()
    SetEntityCoords(PlayerPedId(), 0.0, 0.0, 0.0)
end, false)

RegisterCommand('look', function()
    looks = looks + 1
    local peds = {}
    for _, ped in ipairs(GetGamePool('CPed')) do
        peds[#peds + 1] = ped
    end
    table.sort(peds)
    local exists = vehicle_id ~= nil and DoesEntityExist(vehicle_id)
    local color = 'nil'
    if vehicle_id ~= nil then
        color = tostring(Entity(vehicle_id).state.color)
    end
    TriggerServerEvent('scope:view', ('%d peds %s vehicle %s %s'):format(
        looks, table.concat(peds, ','), tostring(exists), color))
end, false)
";

/// The view body a client reports for the peds it holds and the vehicle, as
/// `peds <ids> vehicle <exists> <colour>`, with the ids in ascending order.
fn view_line(peds: &[u32], vehicle: &str) -> String {
    let mut sorted = peds.to_vec();
    sorted.sort_unstable();
    let list: Vec<String> = sorted.iter().map(u32::to_string).collect();
    format!("peds {} vehicle {vehicle}", list.join(","))
}

/// The look count at the start of a view, or zero when there is none.
fn view_count(view: &str) -> u32 {
    view.split(' ')
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

/// The view without its look count.
fn view_body(view: &str) -> &str {
    view.split_once(' ').map_or("", |(_, body)| body)
}

impl Harness {
    /// The latest view `player` reported.
    fn view(&self, player: u32) -> String {
        let value = self.call("view", &[Value::Int(i64::from(player))]);
        value.as_str().unwrap_or_default().to_owned()
    }

    /// Makes `asked` look until the latest view of `player` is a look made
    /// after this call, and says `expected`.
    fn look_until(
        &self,
        asked: &ClientRuntime,
        runtimes: &[&ClientRuntime],
        player: u32,
        expected: &str,
        what: &str,
    ) {
        let before = view_count(&self.view(player));
        self.ask_until(asked, runtimes, "look", what, |_| {
            let view = self.view(player);
            view_count(&view) > before && view_body(&view) == expected
        });
    }

    /// Creates a vehicle at `position` through the scoping resource.
    fn spawn_vehicle(&self, position: [f64; 3]) -> u32 {
        let args: Vec<Value> = position.into_iter().map(Value::Float).collect();
        self.number("spawn_vehicle", &args)
    }
}

/// Sets `sixm_cullRadius` on the server.
fn set_cull_radius(h: &Harness, radius: u32) {
    h.fx.server
        .execute(&format!("set sixm_cullRadius {radius}"));
}

#[test]
fn scoping_sends_each_player_the_entities_near_it_and_its_own() {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, 50);
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));
    let both_runtimes = [&*a, &*b];
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    let ped_b = h.number("ped_of", &[Value::Int(2)]);

    // Both peds start at the origin, so each player holds both.
    let both = view_line(&[ped_a, ped_b], "false nil");
    h.look_until(&a, &both_runtimes, 1, &both, "a to see both peds");
    h.look_until(&b, &both_runtimes, 2, &both, "b to see both peds");

    // `a` walks 1000 metres away: `b` no longer holds its ped, and `a` no longer
    // holds the ped of `b`, but it keeps its own.
    a.handle(ClientInput::Line("far".to_owned()));
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b to lose a's ped",
    );
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "false nil"),
        "a to lose b's ped",
    );
    assert!(b.entity(ped_a).is_none());

    // A vehicle 10 metres from `a` is held by `a` only.
    let vehicle = h.spawn_vehicle([1000.0, 10.0, 0.0]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "true nil"),
        "a to see the vehicle",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b not to see the vehicle",
    );
    assert!(b.entity(vehicle).is_none());

    // The colour is a state bag of the vehicle, so only `a` receives it.
    h.call("paint", &[Value::from("red")]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "true red"),
        "a to see the colour",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b not to see the colour",
    );

    // `a` walks back: `b` holds both peds again, and `a` loses the vehicle.
    a.handle(ClientInput::Line("home".to_owned()));
    h.look_until(&b, &both_runtimes, 2, &both, "b to see a's ped again");
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a, ped_b], "false nil"),
        "a to lose the vehicle",
    );
    assert!(a.entity(vehicle).is_none());
    assert_eq!(
        h.call("ped_of", &[Value::Int(1)]),
        Value::Int(i64::from(ped_a)),
        "a keeps its own ped"
    );
}

#[test]
fn a_client_that_joins_far_from_an_entity_is_not_sent_its_state() {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, 50);
    let vehicle = h.spawn_vehicle([1000.0, 0.0, 0.0]);
    h.call("paint", &[Value::from("red")]);
    let dir = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let ped_a = h.number("ped_of", &[Value::Int(1)]);

    h.look_until(
        &a,
        &[&*a],
        1,
        &view_line(&[ped_a], "false nil"),
        "a to report the vehicle it does not hold",
    );
    assert!(a.entity(vehicle).is_none());
}

#[test]
fn a_radius_of_zero_sends_every_entity_to_every_player() {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, 0);
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));
    let both_runtimes = [&*a, &*b];
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    let ped_b = h.number("ped_of", &[Value::Int(2)]);

    a.handle(ClientInput::Line("far".to_owned()));
    h.spawn_vehicle([1000.0, 10.0, 0.0]);
    h.call("paint", &[Value::from("red")]);
    let everything = view_line(&[ped_a, ped_b], "true red");
    h.look_until(&a, &both_runtimes, 1, &everything, "a to see everything");
    h.look_until(&b, &both_runtimes, 2, &everything, "b to see everything");
    assert!(b.entity(ped_a).is_some());
}

#[test]
fn changing_the_radius_brings_every_client_up_to_date() {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, 50);
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));
    let both_runtimes = [&*a, &*b];
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    let ped_b = h.number("ped_of", &[Value::Int(2)]);
    a.handle(ClientInput::Line("far".to_owned()));
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b to lose a's ped",
    );

    // Scoping off: `b` is sent what it does not hold yet.
    set_cull_radius(&h, 0);
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_a, ped_b], "false nil"),
        "b to see a's ped after scoping is turned off",
    );

    // Scoping on again: `b` loses the ped it is too far from.
    set_cull_radius(&h, 50);
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b to lose a's ped after scoping is turned on again",
    );
}

#[test]
fn a_frame_with_nothing_moving_sends_no_entity_messages() {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, 50);
    let dir = tempfile::tempdir().expect("temp dir");
    let mut client = h.connect_client(&dir.path().join("cache"));
    let vehicle = h.spawn_vehicle([0.0, 0.0, 0.0]);

    // Run frames until the client has the vehicle, so that the creation is not
    // counted below.
    let mut seen: Vec<ClientNotice> = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while !seen
        .iter()
        .any(|notice| matches!(notice, ClientNotice::EntityCreated { id, .. } if *id == vehicle))
    {
        assert!(
            Instant::now() < deadline,
            "the client never got the vehicle"
        );
        h.pump_server();
        h.fx.server.tick(Instant::now());
        drain_notices(&mut client, &mut seen);
        thread::sleep(STEP);
    }

    // Nothing moves from here on, so no frame sends an entity message.
    let mut quiet: Vec<ClientNotice> = Vec::new();
    for _ in 0..40 {
        h.pump_server();
        h.fx.server.tick(Instant::now());
        drain_notices(&mut client, &mut quiet);
        thread::sleep(STEP);
    }
    let entity_messages: Vec<&ClientNotice> = quiet
        .iter()
        .filter(|notice| {
            matches!(
                notice,
                ClientNotice::EntityCreated { .. }
                    | ClientNotice::EntityTransforms { .. }
                    | ClientNotice::EntityDeleted { .. }
                    | ClientNotice::StateBag { .. }
                    | ClientNotice::StateBagRemoved { .. }
            )
        })
        .collect();
    assert!(entity_messages.is_empty(), "{entity_messages:?}");
}

/// Reads every notice the connection has ready, without waiting.
fn drain_notices(client: &mut Client, out: &mut Vec<ClientNotice>) {
    while let Some(notice) = client.poll(Duration::ZERO) {
        out.push(notice);
    }
}

#[test]
fn a_player_holds_only_the_entities_of_its_bucket_while_scoping() {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, 50);
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));
    let both_runtimes = [&*a, &*b];
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    let ped_b = h.number("ped_of", &[Value::Int(2)]);
    let both = view_line(&[ped_a, ped_b], "false nil");
    h.look_until(&a, &both_runtimes, 1, &both, "a to see both peds");
    h.look_until(&b, &both_runtimes, 2, &both, "b to see both peds");

    // Moving `b` to bucket 2 takes its ped along: `a` loses it, `b` loses `a`'s.
    h.call("set_bucket", &[Value::Int(2), Value::Int(2)]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "false nil"),
        "a to lose the ped of bucket 2",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b to lose the ped of bucket 0",
    );

    // A vehicle in bucket 0 reaches `a` only; moved to bucket 2, it reaches `b` only.
    h.spawn_vehicle([0.0, 0.0, 0.0]);
    h.call("paint", &[Value::from("red")]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "true red"),
        "a to see the vehicle of bucket 0",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b not to see the vehicle of bucket 0",
    );
    h.call("set_vehicle_bucket", &[Value::Int(2)]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "false nil"),
        "a to lose the vehicle of bucket 2",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "true red"),
        "b to see the vehicle of bucket 2",
    );

    // Back in bucket 0, `b` sees `a` and its ped again.
    h.call("set_bucket", &[Value::Int(2), Value::Int(0)]);
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_a, ped_b], "false nil"),
        "b to see a and its ped after returning to bucket 0",
    );
}

#[test]
fn with_scoping_off_each_player_holds_the_entities_of_its_bucket() {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, 0);
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));
    let both_runtimes = [&*a, &*b];
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    let ped_b = h.number("ped_of", &[Value::Int(2)]);
    let both = view_line(&[ped_a, ped_b], "false nil");
    h.look_until(&a, &both_runtimes, 1, &both, "a to see both peds");
    h.look_until(&b, &both_runtimes, 2, &both, "b to see both peds");

    h.call("set_bucket", &[Value::Int(2), Value::Int(2)]);
    h.spawn_vehicle([0.0, 0.0, 0.0]);
    h.call("paint", &[Value::from("red")]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "true red"),
        "a to see its bucket: its ped and the vehicle of bucket 0",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b to see only its own ped",
    );
}

/// A vehicle in bucket 2 with a colour exists before a player joins bucket 0.
/// The join snapshots of the entities and of the state must not carry it.
fn a_player_joining_does_not_get_the_entities_of_other_buckets(radius: u32) {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, radius);
    let vehicle = h.spawn_vehicle([0.0, 0.0, 0.0]);
    h.call("paint", &[Value::from("red")]);
    h.call("set_vehicle_bucket", &[Value::Int(2)]);
    let dir = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir.path().join("cache"), &dir.path().join("data"));
    let ped_a = h.number("ped_of", &[Value::Int(1)]);

    h.look_until(
        &a,
        &[&*a],
        1,
        &view_line(&[ped_a], "false nil"),
        "a not to get the vehicle or its colour at join",
    );
    assert!(a.entity(vehicle).is_none());
}

#[test]
fn a_joining_player_gets_no_entities_of_other_buckets_while_scoping() {
    a_player_joining_does_not_get_the_entities_of_other_buckets(50);
}

#[test]
fn a_joining_player_gets_no_entities_of_other_buckets_with_scoping_off() {
    a_player_joining_does_not_get_the_entities_of_other_buckets(0);
}

/// A move and a bucket change in the same frame leave every client with the
/// entities of its final bucket and place.
fn a_bucket_change_and_a_move_in_one_frame_settle(radius: u32) {
    let h = Harness::start(SCOPE_RESOURCE, SCOPE_SERVER, SCOPE_CLIENT, SCOPE_EXPORTS);
    set_cull_radius(&h, radius);
    let dir_a = tempfile::tempdir().expect("temp dir");
    let dir_b = tempfile::tempdir().expect("temp dir");
    let a = h.connect(&dir_a.path().join("cache"), &dir_a.path().join("data"));
    let b = h.connect(&dir_b.path().join("cache"), &dir_b.path().join("data"));
    let both_runtimes = [&*a, &*b];
    let ped_a = h.number("ped_of", &[Value::Int(1)]);
    let ped_b = h.number("ped_of", &[Value::Int(2)]);
    let both = view_line(&[ped_a, ped_b], "false nil");
    h.look_until(&a, &both_runtimes, 1, &both, "a to see both peds");

    // `b` walks away and changes bucket in the same frame: nobody sees `b`.
    b.handle(ClientInput::Line("far".to_owned()));
    h.call("set_bucket", &[Value::Int(2), Value::Int(3)]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &view_line(&[ped_a], "false nil"),
        "a to lose the ped of b",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &view_line(&[ped_b], "false nil"),
        "b to lose the ped of a",
    );

    // `b` comes back and `a` joins its bucket in the same frame: both see both.
    b.handle(ClientInput::Line("home".to_owned()));
    h.call("set_bucket", &[Value::Int(1), Value::Int(3)]);
    h.look_until(
        &a,
        &both_runtimes,
        1,
        &both,
        "a to see b after meeting in bucket 3",
    );
    h.look_until(
        &b,
        &both_runtimes,
        2,
        &both,
        "b to see a after meeting in bucket 3",
    );
}

#[test]
fn a_bucket_change_and_a_move_in_one_frame_settle_while_scoping() {
    a_bucket_change_and_a_move_in_one_frame_settle(50);
}

#[test]
fn a_bucket_change_and_a_move_in_one_frame_settle_with_scoping_off() {
    a_bucket_change_and_a_move_in_one_frame_settle(0);
}
