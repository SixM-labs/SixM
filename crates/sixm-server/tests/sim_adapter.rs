//! End-to-end tests for [`GameAdapter`] and [`SimAdapter`]:
//! - Headless client leaves `Citizen.InvokeNative` absent.
//! - Adapter natives and `Citizen.InvokeNative` work in both Lua and JS.
//! - `SimAdapter` moves an owned entity via `TaskGoStraightToCoord`, replicates
//!   movement across the server to another client, stops on `FreezeEntityPosition`,
//!   and snaps cleanly on server teleport.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use common::Fixture;
use sixm_client::{Client, ClientRuntime, SimAdapter};
use sixm_core::{
    AdapterError, AdapterSink, ClientTarget, GameAdapter, Host, MirrorChange, NativeRegistry,
    ServerHost, Value,
};
use sixm_net::Identity;
use sixm_server::Input;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(15);
const STEP: Duration = Duration::from_millis(5);

const SRV_SCRIPT: &str = r"
local log = {}
local peds = {}
AddEventHandler('playerJoining', function()
    local id = tonumber(source)
    peds[id] = GetPlayerPed(id)
end)
RegisterNetEvent('sim:log', function(msg)
    log[#log + 1] = ('%s:%s'):format(tostring(source), msg)
end)
exports('report', function() return table.concat(log, '\n') end)
exports('driver', function() return peds[1] or 0 end)
exports('driverCoords', function()
    local p = peds[1]
    if not p then return 'none' end
    local c = GetEntityCoords(p)
    return ('%.2f,%.2f,%.2f'):format(c.x, c.y, c.z)
end)
exports('freezePed', function(toggle)
    if peds[1] then FreezeEntityPosition(peds[1], toggle) end
end)
exports('teleportPed', function(x, y, z)
    if peds[1] then SetEntityCoords(peds[1], x, y, z) end
end)
";

const CLI_LUA: &str = r"
local hasInvoke = Citizen.InvokeNative ~= nil
TriggerServerEvent('sim:log', 'lua:invoke=' .. tostring(hasInvoke))
if hasInvoke then
    local okBad = pcall(function()
        Citizen.InvokeNative(0x12345678)
    end)
    TriggerServerEvent('sim:log', 'lua:badHash=' .. tostring(okBad))
end
RegisterNetEvent('sim:driveLua', function(x, y, z, speed)
    local ped = PlayerPedId()
    TaskGoStraightToCoord(ped, x, y, z, speed)
    local viaHash = Citizen.InvokeNative(GetHashKey('GetSimEntityCoords'), ped)
    TriggerServerEvent('sim:log', ('lua:started=%.1f'):format(viaHash.x))
end)
";

const CLI_JS: &str = r#"
const hasInvoke = typeof Citizen.InvokeNative === "function";
TriggerServerEvent("sim:log", `js:invoke=${hasInvoke}`);
RegisterNetEvent("sim:checkSpeedJs", () => {
    const ped = PlayerPedId();
    const s = GetEntitySpeed(ped);
    const c = Citizen.InvokeNative(GetHashKey("GetSimEntityCoords"), ped);
    TriggerServerEvent("sim:log", `js:speed=${s.toFixed(1)}@${c.x.toFixed(1)}`);
});
"#;

const SPAWNMGR: &str =
    include_str!("../../../examples/server-data/resources/[system]/spawnmanager/client.lua");
const SPAWNJSON: &str =
    include_str!("../../../examples/server-data/resources/[system]/spawnmanager/json.js");
const MANIFEST: &str =
    include_str!("../../../examples/server-data/resources/[system]/spawnmanager/fxmanifest.lua");
const REPORT_SERVER: &str = r"
local report = ''
local peds = {}
AddEventHandler('playerJoining', function()
local player = tonumber(source)
peds[player] = GetPlayerPed(player)
end)
RegisterNetEvent('spawn:report', function(value) report = value end)
exports('report', function() return report end)
exports('ped', function(player) return peds[player] or 0 end)
";
const CALLER: &str = r#"
if (GetPlayerServerId(PlayerId()) === 1) {
    try {
        const results = {};
        const eventInfos = [];
        let callbackInfo;
        on("playerSpawned", info => {
            eventInfos.push(info);
            results.eventInfo = info;
        });
        const sm = exports.spawnmanager;
        const first = sm.addSpawnPoint({ x: 31, y: 4, z: 2, heading: 78 });
        sm.setAutoSpawnCallback(() => {
            results.auto = (results.auto || 0) + 1;
            const ped = PlayerPedId();
            results.autoPed = [ped, DoesEntityExist(ped), IsEntityPositionFrozen(ped), IsEntityAttached(ped)];
            results.autoSpawn = sm.spawnPlayer(first);
        });
        sm.forceRespawn();
        sm.setAutoSpawn(true);
        sm.forceRespawn();
        const loaded = sm.loadSpawns('{"spawns":[{"x":40,"y":1,"z":2,"heading":10}]}');
        let malformed = false;
        try {
            sm.loadSpawns('{"spawns":[{"x":1,"y":2,"z":3,"heading":0},null,{"x":4,"y":5,"z":6,"heading":0}]}');
        } catch (_) { malformed = true; }
        let trailingNullRejected = false;
        try {
            sm.loadSpawns('{"spawns":[{"x":1,"y":2,"z":3,"heading":0},null]}');
        } catch (_) { trailingNullRejected = true; }
        let objectSpawnsRejected = false;
        try { sm.loadSpawns('{"spawns":{}}'); }
        catch (_) { objectSpawnsRejected = true; }
        const third = sm.addSpawnPoint({ x: 50, y: 8, z: 3, heading: 90 });
        const unknownRemoved = !sm.removeSpawnPoint(99999);
        const removed = sm.removeSpawnPoint(third);
        const removedAgain = !sm.removeSpawnPoint(third);
        const unknownSpawn = !sm.spawnPlayer(99999);
        const direct = sm.spawnPlayer({ x: 60, y: 9, z: 4, heading: 123, skipFade: true });
        const randomSpawn = sm.spawnPlayer(undefined, info => { results.randomInfo = info; });
        let callbackCount = 0;
        const explicit = sm.spawnPlayer(first, info => { callbackCount++; callbackInfo = info; });
        let noPoint = false;
        setTimeout(() => {
            sm.forceRespawn();
            sm.forceRespawn();
            setTimeout(() => {
                noPoint = sm.removeSpawnPoint(first) && sm.removeSpawnPoint(2) && !sm.spawnPlayer();
                sm.setAutoSpawn(false);
                sm.forceRespawn();
                setTimeout(() => {
                    const info = results.eventInfo || {};
                    const callback = callbackInfo || {};
                    const randomInfo = results.randomInfo || {};
                    const adapter = Citizen.InvokeNative(GetHashKey("GetSimEntityCoords"), PlayerPedId());
                    const report = [first, loaded, malformed, trailingNullRejected, objectSpawnsRejected,
                        third, unknownRemoved, removed,
                        removedAgain, explicit, unknownSpawn, direct, randomSpawn, noPoint,
                        results.auto, results.autoSpawn, ...results.autoPed, eventInfos.length, callbackCount,
                        randomInfo.idx === 1 || randomInfo.idx === 2,
                        info.idx, callback.idx,
                        info.idx === callback.idx && info.x === callback.x && info.y === callback.y
                            && info.z === callback.z && info.heading === callback.heading,
                        info.x, info.y, info.z, info.heading, adapter.x, adapter.y, adapter.z,
                        (() => { try { sm.spawnPlayer({ x: 1, y: 2, z: 3, heading: NaN }); return false; }
                            catch (_) { return true; } })()]
                        .map(String).join("|");
                    TriggerServerEvent("spawn:report", report);
                }, 20);
            }, 100);
        }, 5);
    } catch (err) {
        TriggerServerEvent("spawn:report", `ERR:${String(err)}`);
    }
}
"#;
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

    fn pump_clients(&self, a: &ClientRuntime, b: Option<&ClientRuntime>, now: Instant) {
        self.pump_server();
        a.pump(Duration::ZERO);
        a.tick(now);
        if let Some(other) = b {
            other.pump(Duration::ZERO);
            other.tick(now);
        }
    }

    fn driver_ped_id(&self) -> u32 {
        match self
            .fx
            .manager()
            .call_export("simtest", "driver", &[], None)
            .expect("driver")[0]
        {
            Value::Int(id) => u32::try_from(id).expect("ped id"),
            ref other => panic!("unexpected ped id: {other:?}"),
        }
    }
}

struct FailingAdapter {
    natives: NativeRegistry,
}

impl GameAdapter for FailingAdapter {
    fn attach(&mut self, _sink: AdapterSink) -> Result<(), AdapterError> {
        Err(AdapterError::NotReady)
    }
    fn detach(&mut self) {}
    fn before_scripts(&mut self, _now: Instant) {}
    fn after_scripts(&mut self, _now: Instant) {}
    fn apply_changes(&mut self, _changes: &[MirrorChange], _errors: &mut Vec<(u32, AdapterError)>) {
    }
    fn read_owned(&mut self, _owned: &[u32], _out: &mut Vec<(u32, [f32; 3], [f32; 3])>) {}
    fn natives(&self) -> &NativeRegistry {
        &self.natives
    }
}

#[test]
fn sim_adapter_exposes_natives_to_lua_and_js_and_replicates_movement() {
    let lb = Loopback::new(
        "simtest",
        "fx_version 'cerulean'\ngame 'common'\n\
         server_script 'srv.lua'\nclient_script 'cli.lua'\nclient_script 'cli.js'\n\
         server_export 'report'\nserver_export 'freezePed'\nserver_export 'teleportPed'\n\
         server_export 'driverCoords'\nserver_export 'driver'\n",
        &[
            ("srv.lua", SRV_SCRIPT),
            ("cli.lua", CLI_LUA),
            ("cli.js", CLI_JS),
        ],
    );

    let (cache_a, data_a) = (TempDir::new().expect("ca"), TempDir::new().expect("da"));
    let rt_a: Rc<ClientRuntime> =
        ClientRuntime::new(lb.connect("Driver", cache_a.path()), data_a.path());
    rt_a.set_adapter(Box::new(SimAdapter::new()));
    rt_a.try_start_synced().expect("adapter attaches");

    let (cache_b, data_b) = (TempDir::new().expect("cb"), TempDir::new().expect("db"));
    let rt_b: Rc<ClientRuntime> =
        ClientRuntime::new(lb.connect("Observer", cache_b.path()), data_b.path());
    rt_b.start_synced();

    let mut now = Instant::now();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        lb.pump_clients(&rt_a, Some(&rt_b), now);
        let log = lb.fx.text("simtest", "report");
        if [
            "1:lua:invoke=true",
            "1:lua:badHash=false",
            "1:js:invoke=true",
            "2:lua:invoke=false",
            "2:js:invoke=false",
        ]
        .into_iter()
        .all(|needle| log.contains(needle))
        {
            break;
        }
        assert!(Instant::now() < deadline, "boot log mismatch: {log}");
        thread::sleep(STEP);
        now += Duration::from_millis(50);
    }

    lb.fx
        .manager()
        .trigger_client_event(
            "sim:driveLua",
            ClientTarget::Player(1),
            &[
                Value::Float(20.0),
                Value::Float(0.0),
                Value::Float(0.0),
                Value::Float(10.0),
            ],
        )
        .expect("send driveLua");

    let step_dt = Duration::from_millis(100);
    for idx in 0..8 {
        now += step_dt;
        lb.pump_clients(&rt_a, Some(&rt_b), now);
        if idx == 2 {
            lb.fx
                .manager()
                .trigger_client_event("sim:checkSpeedJs", ClientTarget::Player(1), &[])
                .expect("send checkSpeedJs");
        }
        thread::sleep(STEP);
    }

    let ped_id = lb.driver_ped_id();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        now += step_dt;
        lb.pump_clients(&rt_a, Some(&rt_b), now);
        let coords = lb.fx.text("simtest", "driverCoords");
        let log = lb.fx.text("simtest", "report");
        let obs_x = rt_b.entity(ped_id).map_or(0.0, |info| info.position[0]);
        if coords != "0.00,0.00,0.00" && obs_x > 1.0 && log.contains("js:speed=10.0@") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not replicated: coords={coords} obs_x={obs_x} log={log}"
        );
        thread::sleep(STEP);
    }

    verify_freeze_and_teleport(&lb, &rt_a, ped_id, &mut now);

    rt_a.shutdown();
    rt_b.shutdown();
}

#[test]
fn failing_adapter_attach_returns_error_from_try_start_synced() {
    let lb = Loopback::new(
        "failtest",
        "fx_version 'cerulean'\ngame 'common'\nserver_script 'srv.lua'\n",
        &[("srv.lua", "print('up')")],
    );
    let (cache, data) = (
        TempDir::new().expect("cache"),
        TempDir::new().expect("data"),
    );
    let rt = ClientRuntime::new(lb.connect("FailCli", cache.path()), data.path());
    rt.set_adapter(Box::new(FailingAdapter {
        natives: NativeRegistry::default(),
    }));
    let err = rt.try_start_synced().expect_err("attach fails");
    assert!(matches!(err, AdapterError::NotReady));
    rt.shutdown();
}

#[test]
fn stock_spawnmanager_exports_move_the_local_ped_and_dispatch_callbacks_next_tick() {
    let fx = Fixture::new();
    fx.add_resource(
        "simtest",
        "fx_version 'cerulean'\ngame 'common'\nserver_script 'srv.lua'\nserver_export 'report'\nserver_export 'ped'\n",
        &[("srv.lua", REPORT_SERVER)],
    );
    fx.add_resource(
        "spawnmanager",
        MANIFEST,
        &[("client.lua", SPAWNMGR), ("json.js", SPAWNJSON)],
    );
    fx.add_resource(
        "spawncaller",
        "fx_version 'cerulean'\ngame 'common'\nclient_script 'client.js'\ndependency 'spawnmanager'\n",
        &[("client.js", CALLER)],
    );
    fx.server.execute("endpoint_add_udp 127.0.0.1:0");
    let (tx, input) = mpsc::channel();
    let addr = fx.server.start_network(tx).expect("loopback server");
    fx.server.execute("ensure simtest");
    fx.server.execute("ensure spawncaller");
    let lb = Loopback { fx, input, addr };

    let (cache, data) = (
        TempDir::new().expect("cache"),
        TempDir::new().expect("data"),
    );
    let rt_a = ClientRuntime::new(lb.connect("SpawnerA", cache.path()), data.path());
    rt_a.set_adapter(Box::new(SimAdapter::new()));
    rt_a.try_start_synced().expect("adapter A attaches");
    let (cache_b, data_b) = (
        TempDir::new().expect("cache B"),
        TempDir::new().expect("data B"),
    );
    let rt_b = ClientRuntime::new(lb.connect("SpawnerB", cache_b.path()), data_b.path());
    rt_b.set_adapter(Box::new(SimAdapter::new()));
    rt_b.try_start_synced().expect("adapter B attaches");
    let other_ped = match &lb
        .fx
        .manager()
        .call_export("simtest", "ped", &[Value::Int(2)], None)
        .unwrap()[0]
    {
        Value::Int(id) => u32::try_from(*id).expect("other ped id"),
        other => panic!("unexpected other ped handle {other:?}"),
    };
    let other_before = rt_b
        .entity(other_ped)
        .expect("second client's local ped is mirrored")
        .position;

    let mut now = Instant::now();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        lb.pump_clients(&rt_a, Some(&rt_b), now);
        let report = lb.fx.text("simtest", "report");
        if !report.is_empty() {
            assert_eq!(
                report,
                "1|true|true|true|true|3|true|true|true|true|true|true|true|true|3|true|1|true|false|false|6|1|true|1|1|true|31|4|2|78|31|4|2|true"
            );
            let ped = match &lb
                .fx
                .manager()
                .call_export("simtest", "ped", &[Value::Int(1)], None)
                .unwrap()[0]
            {
                Value::Int(id) => u32::try_from(*id).expect("ped id"),
                other => panic!("unexpected ped handle {other:?}"),
            };
            let info = rt_a.entity(ped).expect("SimAdapter mirrors local ped");
            assert_eq!(info.position, [31.0, 4.0, 2.0]);
            assert_eq!(info.rotation[2], 78.0);
            assert_eq!(
                rt_b.entity(other_ped)
                    .expect("other ped remains mirrored")
                    .position,
                other_before
            );
            break;
        }
        assert!(Instant::now() < deadline, "spawnmanager report timed out");
        thread::sleep(STEP);
        now += Duration::from_millis(50);
    }
    rt_a.shutdown();
    rt_b.shutdown();
}

fn verify_freeze_and_teleport(lb: &Loopback, rt_a: &ClientRuntime, ped_id: u32, now: &mut Instant) {
    let step_dt = Duration::from_millis(100);
    lb.fx
        .manager()
        .call_export("simtest", "freezePed", &[Value::Bool(true)], None)
        .expect("freeze");
    for _ in 0..5 {
        *now += step_dt;
        lb.pump_clients(rt_a, None, *now);
        thread::sleep(STEP);
    }
    let frozen_coords = lb.fx.text("simtest", "driverCoords");
    for _ in 0..5 {
        *now += step_dt;
        lb.pump_clients(rt_a, None, *now);
        thread::sleep(STEP);
    }
    assert_eq!(lb.fx.text("simtest", "driverCoords"), frozen_coords);

    lb.fx
        .manager()
        .call_export(
            "simtest",
            "teleportPed",
            &[Value::Float(100.0), Value::Float(5.0), Value::Float(2.0)],
            None,
        )
        .expect("teleport");
    let deadline = Instant::now() + TIMEOUT;
    while !rt_a
        .entity(ped_id)
        .is_some_and(|info| (info.position[0] - 100.0).abs() < 0.05)
    {
        assert!(
            Instant::now() < deadline,
            "teleport did not reach SimAdapter"
        );
        *now += step_dt;
        lb.pump_clients(rt_a, None, *now);
        thread::sleep(STEP);
    }
}
