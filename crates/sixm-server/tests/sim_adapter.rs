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
local anchor
AddEventHandler('playerJoining', function()
local player = tonumber(source)
peds[player] = GetPlayerPed(player)
end)
RegisterNetEvent('spawn:report', function(value) report = value end)
exports('report', function() return report end)
exports('ped', function(player) return peds[player] or 0 end)
exports('freezePed', function(player, toggle)
    if peds[player] then FreezeEntityPosition(peds[player], toggle) end
end)
exports('attachPed', function(player, attach)
    if not peds[player] then return end
    if attach then
        anchor = anchor or CreateObject('prop_test', 0, 0, 0)
        AttachEntityToEntity(peds[player], anchor, 0, 0, 0, 0, 0, 0, 0)
    else
        DetachEntity(peds[player])
    end
end)
";
const MOCK_PED_LUA: &str = r"
local realPlayerPedId = PlayerPedId
local pedReady = false
function PlayerPedId()
    if pedReady then return realPlayerPedId() end
    return 0
end
RegisterNetEvent('mock:enablePed', function()
    pedReady = true
end)
";
const MOCK_MANIFEST: &str = r"fx_version 'cerulean'
game 'common'
client_scripts { 'mock_ped.lua', 'client.lua', 'json.js' }
export 'spawnPlayer'
export 'addSpawnPoint'
export 'removeSpawnPoint'
export 'loadSpawns'
export 'setAutoSpawn'
export 'setAutoSpawnCallback'
export 'forceRespawn'
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

struct SpawnHarness {
    lb: Loopback,
    rt: Rc<ClientRuntime>,
    _cache: TempDir,
    _data: TempDir,
    ped: u32,
}

impl SpawnHarness {
    fn new(use_mock_ped: bool, caller_script: &str) -> Self {
        let fx = Fixture::new();
        fx.add_resource(
            "simtest",
            "fx_version 'cerulean'\ngame 'common'\n\
             server_script 'srv.lua'\n\
             server_export 'report'\nserver_export 'ped'\nserver_export 'freezePed'\nserver_export 'attachPed'\n",
            &[("srv.lua", REPORT_SERVER)],
        );
        if use_mock_ped {
            fx.add_resource(
                "spawnmanager",
                MOCK_MANIFEST,
                &[
                    ("mock_ped.lua", MOCK_PED_LUA),
                    ("client.lua", SPAWNMGR),
                    ("json.js", SPAWNJSON),
                ],
            );
        } else {
            fx.add_resource(
                "spawnmanager",
                MANIFEST,
                &[("client.lua", SPAWNMGR), ("json.js", SPAWNJSON)],
            );
        }
        fx.add_resource(
            "spawncaller",
            "fx_version 'cerulean'\ngame 'common'\nclient_script 'client.lua'\ndependency 'spawnmanager'\n",
            &[("client.lua", caller_script)],
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
        let rt = ClientRuntime::new(lb.connect("Spawner", cache.path()), data.path());
        rt.set_adapter(Box::new(SimAdapter::new()));
        rt.try_start_synced().expect("adapter attaches");
        let ped = match &lb
            .fx
            .manager()
            .call_export("simtest", "ped", &[Value::Int(1)], None)
            .expect("ped export")[0]
        {
            Value::Int(id) => u32::try_from(*id).expect("ped id"),
            other => panic!("unexpected ped handle {other:?}"),
        };
        Self {
            lb,
            rt,
            _cache: cache,
            _data: data,
            ped,
        }
    }

    fn pump_for(&self, duration: Duration) {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            self.lb.pump_clients(&self.rt, None, Instant::now());
            thread::sleep(STEP);
        }
        self.lb.pump_clients(&self.rt, None, Instant::now());
    }

    fn wait_report_prefix(&self, prefix: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.lb.pump_clients(&self.rt, None, Instant::now());
            let report = self.lb.fx.text("simtest", "report");
            if !report.is_empty() && (prefix.is_empty() || report.starts_with(prefix)) {
                return report;
            }
            assert!(
                Instant::now() < deadline,
                "spawnmanager report timed out (prefix={prefix:?}, last={report:?})"
            );
            thread::sleep(STEP);
        }
    }

    fn shutdown(self) {
        self.rt.shutdown();
    }
}

#[test]
fn spawnmanager_frozen_ped_blocks_spawn_until_unfrozen() {
    let caller = r"
local events, cbs = 0, 0
local lastInfo
AddEventHandler('playerSpawned', function(info)
    events = events + 1
    lastInfo = info
end)
RegisterNetEvent('test:spawnWhenFrozen', function()
    local sm = exports.spawnmanager
    local frozen = IsEntityPositionFrozen(PlayerPedId())
    local res = sm:spawnPlayer({ x = 55, y = 66, z = 77, heading = 90 }, function()
        cbs = cbs + 1
    end)
    SetTimeout(30, function()
        TriggerServerEvent('spawn:report', ('phase1:%s|%s|%d|%d'):format(
            tostring(frozen), tostring(res), events, cbs))
    end)
end)
RegisterNetEvent('test:spawnWhenUnfrozen', function()
    local sm = exports.spawnmanager
    local frozen = IsEntityPositionFrozen(PlayerPedId())
    local res = sm:spawnPlayer({ x = 55, y = 66, z = 77, heading = 90 }, function(info)
        cbs = cbs + 1
        lastInfo = info
    end)
    SetTimeout(30, function()
        local info = lastInfo or {}
        TriggerServerEvent('spawn:report', ('phase2:%s|%s|%d|%d|%.0f|%.0f|%.0f|%.0f'):format(
            tostring(frozen), tostring(res), events, cbs,
            info.x or -1, info.y or -1, info.z or -1, info.heading or -1))
    end)
end)
";
    let h = SpawnHarness::new(false, caller);
    h.pump_for(Duration::from_millis(30));
    let before = h.rt.entity(h.ped).expect("ped mirrored").position;

    h.lb.fx
        .manager()
        .call_export(
            "simtest",
            "freezePed",
            &[Value::Int(1), Value::Bool(true)],
            None,
        )
        .expect("freeze ped");
    h.pump_for(Duration::from_millis(40));
    h.lb.fx
        .manager()
        .trigger_client_event("test:spawnWhenFrozen", ClientTarget::Player(1), &[])
        .expect("trigger frozen spawn");
    let phase1 = h.wait_report_prefix("phase1:");
    assert_eq!(phase1, "phase1:true|false|0|0");
    assert_eq!(h.rt.entity(h.ped).expect("ped mirrored").position, before);

    h.lb.fx
        .manager()
        .call_export(
            "simtest",
            "freezePed",
            &[Value::Int(1), Value::Bool(false)],
            None,
        )
        .expect("unfreeze ped");
    h.pump_for(Duration::from_millis(40));
    h.lb.fx
        .manager()
        .trigger_client_event("test:spawnWhenUnfrozen", ClientTarget::Player(1), &[])
        .expect("trigger unfrozen spawn");
    let phase2 = h.wait_report_prefix("phase2:");
    assert_eq!(phase2, "phase2:false|true|1|1|55|66|77|90");
    let info = h.rt.entity(h.ped).expect("ped mirrored");
    assert_eq!(info.position, [55.0, 66.0, 77.0]);
    assert_eq!(info.rotation[2], 90.0);
    h.shutdown();
}

#[test]
fn spawnmanager_missing_ped_blocks_spawn_and_cancels_retry_on_disable() {
    let caller = r"
local events, cbs = 0, 0
AddEventHandler('playerSpawned', function() events = events + 1 end)
local sm = exports.spawnmanager
local id = sm:addSpawnPoint({ x = 15, y = 25, z = 35, heading = 45 })
local r1 = sm:spawnPlayer(nil, function() cbs = cbs + 1 end)
local r2 = sm:spawnPlayer(id, function() cbs = cbs + 1 end)
local r3 = sm:spawnPlayer({ x = 15, y = 25, z = 35, heading = 45 }, function() cbs = cbs + 1 end)
sm:setAutoSpawn(true)
SetTimeout(30, function()
    sm:setAutoSpawn(false)
    TriggerEvent('mock:enablePed')
    SetTimeout(50, function()
        TriggerServerEvent('spawn:report', ('%s|%s|%s|%d|%d|%d'):format(
            tostring(r1), tostring(r2), tostring(r3), events, cbs, PlayerPedId()))
    end)
end)
";
    let h = SpawnHarness::new(true, caller);
    let before = h.rt.entity(h.ped).expect("ped mirrored").position;
    let report = h.wait_report_prefix("");
    assert_eq!(report, format!("false|false|false|0|0|{}", h.ped));
    assert_eq!(h.rt.entity(h.ped).expect("ped mirrored").position, before);
    h.shutdown();
}

#[test]
fn spawnmanager_deferred_ped_retries_until_valid_then_spawns_once() {
    let caller = r"
local events = {}
AddEventHandler('playerSpawned', function(info) events[#events + 1] = info end)
local sm = exports.spawnmanager
sm:addSpawnPoint({ x = 22, y = 33, z = 44, heading = 60 })
sm:setAutoSpawn(true)
SetTimeout(30, function()
    local beforeEnable = #events
    TriggerEvent('mock:enablePed')
    SetTimeout(50, function()
        local e = events[1] or {}
        TriggerServerEvent('spawn:report', ('%d|%d|%s|%.0f|%.0f|%.0f|%.0f|%d'):format(
            beforeEnable, #events, tostring(e.idx),
            e.x or -1, e.y or -1, e.z or -1, e.heading or -1, PlayerPedId()))
    end)
end)
";
    let h = SpawnHarness::new(true, caller);
    let report = h.wait_report_prefix("");
    assert_eq!(report, format!("0|1|1|22|33|44|60|{}", h.ped));
    let info = h.rt.entity(h.ped).expect("ped mirrored");
    assert_eq!(info.position, [22.0, 33.0, 44.0]);
    assert_eq!(info.rotation[2], 60.0);
    h.shutdown();
}

#[test]
fn spawnmanager_deferred_config_and_lua_consumers() {
    let caller = r"
local events = {}
AddEventHandler('playerSpawned', function(info) events[#events + 1] = info end)
local sm = exports.spawnmanager
sm:setAutoSpawn(true)
SetTimeout(30, function()
    local beforePoint = #events
    sm:addSpawnPoint({ x = 12, y = 24, z = 36, heading = 90, skipFade = true })
    SetTimeout(40, function()
        local afterPoint = #events
        local id2 = sm:addSpawnPoint({ x = 42, y = 48, z = 54, heading = 135, model = 'mp_m_freemode_01' })
        local cbCount = 0
        local cbInfo
        local spawned = sm:spawnPlayer(id2, function(info)
            cbCount = cbCount + 1
            cbInfo = info
        end)
        SetTimeout(30, function()
            local e1 = events[1] or {}
            local e2 = events[2] or {}
            local cb = cbInfo or {}
            local same = e2.idx == cb.idx and e2.x == cb.x and e2.y == cb.y
                and e2.z == cb.z and e2.heading == cb.heading
                and e2.model == cb.model and e2.skipFade == cb.skipFade
            TriggerServerEvent('spawn:report', ('%d|%d|%d|%d|%s|%s|%s|%d|%s|%s|%s'):format(
                beforePoint, afterPoint, #events, cbCount,
                tostring(e1.idx), tostring(e1.skipFade), tostring(spawned),
                id2, tostring(cb.idx), tostring(cb.model == GetHashKey('mp_m_freemode_01')), tostring(same)))
        end)
    end)
end)
";
    let h = SpawnHarness::new(false, caller);
    let report = h.wait_report_prefix("");
    assert_eq!(report, "0|1|2|1|1|true|true|2|2|true|true");
    let info = h.rt.entity(h.ped).expect("ped mirrored");
    assert_eq!(info.position, [42.0, 48.0, 54.0]);
    assert_eq!(info.rotation[2], 135.0);
    h.shutdown();
}

#[test]
fn spawnmanager_non_spawning_callback_latches_without_event() {
    let caller = r"
local events, cbs = 0, 0
AddEventHandler('playerSpawned', function() events = events + 1 end)
local sm = exports.spawnmanager
sm:setAutoSpawnCallback(function()
    cbs = cbs + 1
end)
sm:setAutoSpawn(true)
SetTimeout(30, function()
    sm:addSpawnPoint({ x = 70, y = 80, z = 90, heading = 45 })
    sm:setAutoSpawn(true)
    SetTimeout(30, function()
        TriggerServerEvent('spawn:report', ('%d|%d'):format(cbs, events))
    end)
end)
";
    let h = SpawnHarness::new(false, caller);
    let before = h.rt.entity(h.ped).expect("ped mirrored").position;
    let report = h.wait_report_prefix("");
    assert_eq!(report, "1|0");
    assert_eq!(h.rt.entity(h.ped).expect("ped mirrored").position, before);
    h.shutdown();
}

#[test]
fn spawnmanager_rejects_invalid_inputs_and_preserves_ped_coords() {
    let caller = r#"
local events = 0
AddEventHandler('playerSpawned', function() events = events + 1 end)
local sm = exports.spawnmanager

local function mustFail(label, needle, fn)
    local ok, err = pcall(fn)
    if ok then error(label .. ' unexpectedly succeeded') end
    if needle and not tostring(err):find(needle, 1, true) then
        error(label .. ' unexpected error: ' .. tostring(err))
    end
end

local ok, err = pcall(function()
    mustFail('nil point bad cb', 'callback must be a function', function() sm:spawnPlayer(nil, 123) end)
    mustFail('unknown id bad cb', 'callback must be a function', function() sm:spawnPlayer(99999, 'bad') end)
    mustFail('object bad cb', 'callback must be a function', function()
        sm:spawnPlayer({ x = 10, y = 20, z = 30, heading = 40 }, 'bad')
    end)
    mustFail('nan x', 'finite', function() sm:spawnPlayer({ x = 0/0, y = 0, z = 0, heading = 0 }) end)
    mustFail('inf x', 'finite', function() sm:spawnPlayer({ x = math.huge, y = 0, z = 0, heading = 0 }) end)
    mustFail('-inf x', 'finite', function() sm:spawnPlayer({ x = -math.huge, y = 0, z = 0, heading = 0 }) end)
    mustFail('overflow x', 'finite', function() sm:spawnPlayer({ x = 4e38, y = 0, z = 0, heading = 0 }) end)
    mustFail('neg id', 'positive integer', function() sm:spawnPlayer(-1) end)
    mustFail('float id', 'positive integer', function() sm:spawnPlayer(1.5) end)
    mustFail('huge id', 'positive integer', function() sm:spawnPlayer(1e20) end)
    mustFail('string arg', 'spawn must be an object', function() sm:spawnPlayer('bad') end)
    mustFail('add non-table', 'spawn must be an object', function() sm:addSpawnPoint('bad') end)
    mustFail('add string coord', 'finite', function() sm:addSpawnPoint({ x = '1', y = 2, z = 3, heading = 0 }) end)
    mustFail('add empty model', 'model', function() sm:addSpawnPoint({ x = 1, y = 2, z = 3, heading = 0, model = '' }) end)
    mustFail('add float model', 'model', function() sm:addSpawnPoint({ x = 1, y = 2, z = 3, heading = 0, model = 1.5 }) end)
    mustFail('add huge model', 'model', function() sm:addSpawnPoint({ x = 1, y = 2, z = 3, heading = 0, model = 1e20 }) end)
    mustFail('add bad skipFade', 'skipFade', function() sm:addSpawnPoint({ x = 1, y = 2, z = 3, heading = 0, skipFade = 'yes' }) end)
    mustFail('remove 0', 'positive integer', function() sm:removeSpawnPoint(0) end)
    mustFail('remove -1', 'positive integer', function() sm:removeSpawnPoint(-1) end)
    mustFail('remove 1.5', 'positive integer', function() sm:removeSpawnPoint(1.5) end)
    mustFail('remove huge', 'positive integer', function() sm:removeSpawnPoint(1e20) end)
    mustFail('remove str', 'positive integer', function() sm:removeSpawnPoint('1') end)
    mustFail('private parseSpawnArray', 'private to spawnmanager', function()
        sm.parseSpawnArray('{"spawns":[]}')
    end)
    mustFail('load non-str', 'JSON string', function() sm:loadSpawns(123) end)
    mustFail('load bad json', 'invalid JSON', function() sm:loadSpawns('{bad') end)
    mustFail('load partial bad', 'finite', function()
        sm:loadSpawns('{"spawns":[{"x":1,"y":2,"z":3,"heading":0},{"x":1,"y":2,"z":3,"heading":"bad"}]}')
    end)
    mustFail('load overflow', 'finite', function()
        sm:loadSpawns('{"spawns":[{"x":1e39,"y":2,"z":3,"heading":0}]}')
    end)
    if sm:spawnPlayer() ~= false then error('failed loadSpawns mutated points') end
    mustFail('autoSpawn str', 'boolean', function() sm:setAutoSpawn('true') end)
    mustFail('autoSpawnCb str', 'function', function() sm:setAutoSpawnCallback('bad') end)
    mustFail('autoSpawnCb nil', 'function', function() sm:setAutoSpawnCallback(nil) end)
end)

SetTimeout(20, function()
    if ok then
        TriggerServerEvent('spawn:report', ('ok|%d'):format(events))
    else
        TriggerServerEvent('spawn:report', 'ERR:' .. tostring(err))
    end
end)
"#;
    let h = SpawnHarness::new(false, caller);
    let before = h.rt.entity(h.ped).expect("ped mirrored");
    let report = h.wait_report_prefix("");
    assert_eq!(report, "ok|0");
    let after = h.rt.entity(h.ped).expect("ped mirrored");
    assert_eq!(after.position, before.position);
    assert_eq!(after.rotation, before.rotation);
    h.shutdown();
}

#[test]
fn spawnmanager_direct_spawn_numeric_models_load_batch_and_event_ordering() {
    let caller = r#"
local trace = {}
local infos = {}
AddEventHandler('playerSpawned', function(info)
    trace[#trace + 1] = 'event'
    infos[#infos + 1] = info
end)
local sm = exports.spawnmanager
local pedModel = GetEntityModel(PlayerPedId())

local directOk = sm:spawnPlayer({ x = 11, y = 22, z = 33, heading = 44, skipFade = true }, function(info)
    trace[#trace + 1] = 'cb1'
    infos[#infos + 1] = info
end)
local signedId = sm:addSpawnPoint({ x = 12, y = 23, z = 34, heading = 45, model = -123456 })
local unsignedId = sm:addSpawnPoint({ x = 13, y = 24, z = 35, heading = 46, model = 4294967295 })
local loaded = sm:loadSpawns('{"spawns":[{"x":101,"y":102,"z":103,"heading":10},{"x":201,"y":202,"z":203,"heading":20}]}')

SetTimeout(30, function()
    local directInfo = infos[1] or {}
    local signedOk = sm:spawnPlayer(signedId, function(info)
        trace[#trace + 1] = 'cb2'
        infos[#infos + 1] = info
    end)
    local unsignedOk = sm:spawnPlayer(unsignedId)
    local batch1Ok = sm:spawnPlayer(3.0)
    local batch2Ok = sm:spawnPlayer(4)
    SetTimeout(30, function()
        local signedInfo = infos[3] or {}
        local unsignedInfo = infos[5] or {}
        local batch1Info = infos[6] or {}
        local batch2Info = infos[7] or {}
        TriggerServerEvent('spawn:report', ('%s|%s|%s|%s|%s|%s|%d|%s|%s|%s|%d|%s|%s|%d|%.0f|%.0f|%s|%s'):format(
            tostring(directOk),
            tostring(directInfo.idx == nil),
            tostring(directInfo.skipFade),
            tostring(directInfo.model == pedModel),
            table.concat(trace, ','),
            tostring(loaded),
            signedId,
            tostring(signedOk),
            tostring(math.type(signedInfo.model) == 'integer' and signedInfo.model == -123456),
            tostring(unsignedOk),
            unsignedInfo.model or 0,
            tostring(math.type(unsignedInfo.model) == 'integer'),
            tostring(batch1Ok and batch2Ok),
            batch1Info.idx or -1,
            batch1Info.x or -1,
            batch2Info.x or -1,
            tostring(math.type(batch1Info.idx) == 'integer'),
            tostring(sm:removeSpawnPoint(3.0) and not sm:removeSpawnPoint(3))))
    end)
end)
"#;
    let h = SpawnHarness::new(false, caller);
    let report = h.wait_report_prefix("");
    assert_eq!(
        report,
        "true|true|true|true|event,cb1,event,cb2,event,event,event|true|1|true|true|true|-1|true|true|3|101|201|true|true"
    );
    let info = h.rt.entity(h.ped).expect("ped mirrored");
    assert_eq!(info.position, [201.0, 202.0, 203.0]);
    assert_eq!(info.rotation[2], 20.0);
    h.shutdown();
}

#[test]
fn spawnmanager_force_respawn_without_callback_and_epoch_cancellation() {
    let caller = r"
local events = {}
AddEventHandler('playerSpawned', function(info)
    events[#events + 1] = info
end)
local sm = exports.spawnmanager
sm:addSpawnPoint({ x = 77, y = 88, z = 99, heading = 180 })
sm:setAutoSpawn(true)
sm:forceRespawn()
SetTimeout(30, function()
    local afterFirst = #events
    sm:forceRespawn()
    sm:setAutoSpawn(false)
    sm:setAutoSpawn(true)
    SetTimeout(30, function()
        local afterCancelled = #events
        sm:forceRespawn()
        SetTimeout(30, function()
            local e = events[1] or {}
            TriggerServerEvent('spawn:report', ('%d|%d|%d|%.0f|%.0f|%.0f|%.0f'):format(
                afterFirst, afterCancelled, #events,
                e.x or -1, e.y or -1, e.z or -1, e.heading or -1))
        end)
    end)
end)
";
    let h = SpawnHarness::new(false, caller);
    let report = h.wait_report_prefix("");
    assert_eq!(report, "1|1|2|77|88|99|180");
    let info = h.rt.entity(h.ped).expect("ped mirrored");
    assert_eq!(info.position, [77.0, 88.0, 99.0]);
    assert_eq!(info.rotation[2], 180.0);
    h.shutdown();
}

#[test]
fn spawnmanager_attached_ped_blocks_spawn_until_detached() {
    let caller = r"
local events, cbs = 0, 0
local lastInfo
AddEventHandler('playerSpawned', function(info)
    events = events + 1
    lastInfo = info
end)
RegisterNetEvent('test:spawnWhenAttached', function()
    local sm = exports.spawnmanager
    local attached = IsEntityAttached(PlayerPedId())
    local res = sm:spawnPlayer({ x = 61, y = 72, z = 83, heading = 95 }, function()
        cbs = cbs + 1
    end)
    SetTimeout(30, function()
        TriggerServerEvent('spawn:report', ('phase1:%s|%s|%d|%d'):format(
            tostring(attached), tostring(res), events, cbs))
    end)
end)
RegisterNetEvent('test:spawnWhenDetached', function()
    local sm = exports.spawnmanager
    local attached = IsEntityAttached(PlayerPedId())
    local res = sm:spawnPlayer({ x = 61, y = 72, z = 83, heading = 95 }, function(info)
        cbs = cbs + 1
        lastInfo = info
    end)
    SetTimeout(30, function()
        local info = lastInfo or {}
        TriggerServerEvent('spawn:report', ('phase2:%s|%s|%d|%d|%.0f|%.0f|%.0f|%.0f'):format(
            tostring(attached), tostring(res), events, cbs,
            info.x or -1, info.y or -1, info.z or -1, info.heading or -1))
    end)
end)
";
    let h = SpawnHarness::new(false, caller);
    h.pump_for(Duration::from_millis(30));
    let before = h.rt.entity(h.ped).expect("ped mirrored").position;

    h.lb.fx
        .manager()
        .call_export(
            "simtest",
            "attachPed",
            &[Value::Int(1), Value::Bool(true)],
            None,
        )
        .expect("attach ped");
    h.pump_for(Duration::from_millis(40));
    h.lb.fx
        .manager()
        .trigger_client_event("test:spawnWhenAttached", ClientTarget::Player(1), &[])
        .expect("trigger attached spawn");
    let phase1 = h.wait_report_prefix("phase1:");
    assert_eq!(phase1, "phase1:true|false|0|0");
    assert_eq!(h.rt.entity(h.ped).expect("ped mirrored").position, before);

    h.lb.fx
        .manager()
        .call_export(
            "simtest",
            "attachPed",
            &[Value::Int(1), Value::Bool(false)],
            None,
        )
        .expect("detach ped");
    h.pump_for(Duration::from_millis(40));
    h.lb.fx
        .manager()
        .trigger_client_event("test:spawnWhenDetached", ClientTarget::Player(1), &[])
        .expect("trigger detached spawn");
    let phase2 = h.wait_report_prefix("phase2:");
    assert_eq!(phase2, "phase2:false|true|1|1|61|72|83|95");
    let info = h.rt.entity(h.ped).expect("ped mirrored");
    assert_eq!(info.position, [61.0, 72.0, 83.0]);
    assert_eq!(info.rotation[2], 95.0);
    h.shutdown();
}
