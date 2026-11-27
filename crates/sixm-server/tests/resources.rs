//! Resource natives on the server: lifecycle states seen from scripts, the
//! sorted resource list, manifest metadata and the command, game type and
//! instance natives.

mod common;

use common::{Fixture, manifest};
use sixm_core::{Host, Value};

/// `observer` keeps a log that other resources write to with `record`, read
/// with `log()`, and asks for the state of any resource with `stateOf(name)`.
fn add_observer(fx: &Fixture) {
    fx.add_resource(
        "observer",
        &manifest(&["server.lua"], &["record", "log", "stateOf"], &[]),
        &[(
            "server.lua",
            "LOG = {}\n\
             exports('record', function(entry) LOG[#LOG + 1] = entry end)\n\
             exports('log', function() return table.concat(LOG, ' ') end)\n\
             exports('stateOf', function(name) return GetResourceState(name) end)\n",
        )],
    );
}

/// The state of `resource`, asked through the observer.
fn state_of(fx: &Fixture, resource: &str) -> String {
    let values = fx
        .manager()
        .call_export("observer", "stateOf", &[Value::from(resource)], None)
        .expect("stateOf");
    match values.first() {
        Some(Value::String(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("stateOf returned {other:?}"),
    }
}

/// `probe` records its own state in the observer's log while it loads, starts
/// and stops.
fn add_probe(fx: &Fixture) {
    fx.add_resource(
        "probe",
        &manifest(&["server.lua"], &[], &[]),
        &[(
            "server.lua",
            "local function note(where)\n\
                 exports.observer:record(where .. '=' .. GetResourceState(GetCurrentResourceName()))\n\
             end\n\
             note('load')\n\
             AddEventHandler('onResourceStart', function(name)\n\
                 if name == GetCurrentResourceName() then note('start') end\n\
             end)\n\
             AddEventHandler('onResourceStop', function(name)\n\
                 if name == GetCurrentResourceName() then note('stop') end\n\
             end)\n",
        )],
    );
}

#[test]
fn a_resource_sees_its_own_state_through_start_and_stop() {
    let fx = Fixture::new();
    add_observer(&fx);
    add_probe(&fx);
    fx.manager().start("observer").expect("observer starts");
    assert_eq!(state_of(&fx, "probe"), "stopped");

    fx.manager().start("probe").expect("probe starts");
    assert_eq!(state_of(&fx, "probe"), "started");
    fx.manager().stop("probe").expect("probe stops");

    assert_eq!(state_of(&fx, "probe"), "stopped");
    assert_eq!(
        state_of(&fx, "observer"),
        "started",
        "the observer keeps running"
    );
    let values = fx
        .manager()
        .call_export("observer", "log", &[], None)
        .expect("log");
    assert_eq!(
        values,
        vec![Value::from("load=starting start=started stop=stopping")]
    );
}

#[test]
fn an_unknown_resource_is_missing() {
    let fx = Fixture::new();
    add_observer(&fx);
    fx.manager().start("observer").expect("observer starts");
    assert_eq!(state_of(&fx, "nowhere"), "missing");
}

/// `lister` exports `list()`: the resource count, then the names by index, then
/// the name at the index past the end, which is nil.
fn add_lister(fx: &Fixture) {
    fx.add_resource(
        "lister",
        &manifest(&["server.lua"], &["list"], &[]),
        &[(
            "server.lua",
            "exports('list', function()\n\
                 local out = { tostring(GetNumResources()) }\n\
                 for index = 0, GetNumResources() - 1 do\n\
                     out[#out + 1] = GetResourceByFindIndex(index)\n\
                 end\n\
                 out[#out + 1] = tostring(GetResourceByFindIndex(GetNumResources()))\n\
                 return out\n\
             end)\n",
        )],
    );
}

#[test]
fn resources_are_listed_by_name_whether_or_not_they_run() {
    let fx = Fixture::new();
    add_lister(&fx);
    for name in ["zeta", "alpha", "mid"] {
        fx.add_resource(name, &manifest(&[], &[], &[]), &[]);
    }
    fx.manager().start("lister").expect("lister starts");

    let values = fx
        .manager()
        .call_export("lister", "list", &[], None)
        .expect("list");
    let expected = ["4", "alpha", "lister", "mid", "zeta", "nil"];
    assert_eq!(
        values,
        vec![Value::Array(
            expected.iter().map(|text| Value::from(*text)).collect()
        )]
    );
}

#[test]
fn metadata_is_read_from_the_manifest() {
    let fx = Fixture::new();
    fx.add_resource(
        "versioned",
        "fx_version 'cerulean'\ngame 'common'\nversion '1.2.0'\n",
        &[],
    );
    fx.add_resource(
        "reader",
        &manifest(&["server.lua"], &["read"], &[]),
        &[(
            "server.lua",
            "exports('read', function()\n\
                 return { GetResourceMetadata('versioned', 'version', 0),\n\
                          tostring(GetResourceMetadata('versioned', 'version', 1)),\n\
                          GetNumResourceMetadata('versioned', 'version') }\n\
             end)\n",
        )],
    );
    fx.manager().start("reader").expect("reader starts");

    let values = fx
        .manager()
        .call_export("reader", "read", &[], None)
        .expect("read");
    assert_eq!(
        values,
        vec![Value::Array(vec![
            Value::from("1.2.0"),
            Value::from("nil"),
            Value::Int(1),
        ])]
    );
}

#[test]
fn provide_aliases_work_across_exports_state_metadata_files_and_lifecycle() {
    let fx = Fixture::new();
    fx.add_resource(
        "oxmysql",
        "fx_version 'cerulean'\n\
         game 'common'\n\
         version '2.9.0'\n\
         provide 'mysql-async'\n\
         provides { 'ghmattimysql' }\n\
         server_script 'server.lua'\n\
         server_export 'query'\n",
        &[
            (
                "server.lua",
                "exports('query', function(sql) return 'ok:' .. sql end)\n",
            ),
            ("config.json", "{\"pool\":4}"),
        ],
    );
    fx.add_resource(
        "consumer",
        "fx_version 'cerulean'\n\
         game 'common'\n\
         dependency 'mysql-async'\n\
         server_script 'server.lua'\n\
         server_export 'probe'\n\
         server_export 'names'\n",
        &[(
            "server.lua",
            "exports('probe', function()\n\
                 local a = exports['mysql-async']:query('SELECT 1')\n\
                 local b = exports['ghmattimysql']:query('SELECT 2')\n\
                 local st = GetResourceState('mysql-async')\n\
                 local ver = GetResourceMetadata('mysql-async', 'version', 0)\n\
                 local cfg = LoadResourceFile('mysql-async', 'config.json')\n\
                 return { a, b, st, ver, cfg }\n\
             end)\n\
             exports('names', function()\n\
                 local out = {}\n\
                 for i = 0, GetNumResources() - 1 do\n\
                     out[#out + 1] = GetResourceByFindIndex(i)\n\
                 end\n\
                 return out\n\
             end)\n",
        )],
    );

    fx.manager().refresh();
    assert!(!fx.manager().is_running("mysql-async"));

    // Starting `consumer` starts its dependency `oxmysql` via the `mysql-async` alias.
    fx.manager().start("consumer").expect("consumer starts");
    assert!(fx.manager().is_running("oxmysql"));
    assert!(fx.manager().is_running("mysql-async"));
    assert!(fx.manager().is_running("ghmattimysql"));

    let probe = fx
        .manager()
        .call_export("consumer", "probe", &[], None)
        .expect("probe");
    assert_eq!(
        probe,
        vec![Value::Array(vec![
            Value::from("ok:SELECT 1"),
            Value::from("ok:SELECT 2"),
            Value::from("started"),
            Value::from("2.9.0"),
            Value::from("{\"pool\":4}"),
        ])]
    );

    // GetNumResources / GetResourceByFindIndex keep listing canonical installed names only.
    let names = fx
        .manager()
        .call_export("consumer", "names", &[], None)
        .expect("names");
    assert_eq!(
        names,
        vec![Value::Array(vec![
            Value::from("consumer"),
            Value::from("oxmysql"),
        ])]
    );

    // `call_export` directly via alias works too.
    let direct = fx
        .manager()
        .call_export("mysql-async", "query", &[Value::from("PING")], None)
        .expect("direct alias export");
    assert_eq!(direct, vec![Value::from("ok:PING")]);

    // `restart` and `stop` via alias restart/stop `oxmysql` and its dependent `consumer`.
    fx.manager()
        .restart("mysql-async")
        .expect("restart by alias");
    assert!(fx.manager().is_running("oxmysql"));
    assert!(fx.manager().is_running("consumer"));

    fx.manager().stop("ghmattimysql").expect("stop by alias");
    assert!(!fx.manager().is_running("oxmysql"));
    assert!(!fx.manager().is_running("mysql-async"));
    assert!(!fx.manager().is_running("consumer"));
}

#[test]
fn restart_resource_native_checks_ace_and_supports_aliases() {
    let fx = Fixture::new();
    fx.add_resource(
        "target",
        "fx_version 'cerulean'\n\
         game 'common'\n\
         provide 'target-alias'\n\
         server_script 'server.lua'\n",
        &[(
            "server.lua",
            "GlobalState.boots = (GlobalState.boots or 0) + 1\n",
        )],
    );
    fx.add_resource(
        "controller",
        &manifest(&["server.lua"], &["restartTarget"], &[]),
        &[(
            "server.lua",
            "exports('restartTarget', function(name)\n\
                 return RestartResource(name)\n\
             end)\n",
        )],
    );

    fx.manager().start("target").expect("target starts");
    fx.manager().start("controller").expect("controller starts");
    assert_eq!(
        fx.manager().state_get("global", "boots"),
        Some(Value::Int(1))
    );

    // Denied without `command.restart` ACE.
    let denied = fx
        .manager()
        .call_export(
            "controller",
            "restartTarget",
            &[Value::from("target")],
            None,
        )
        .expect("call export");
    assert_eq!(denied, vec![Value::Bool(false)]);
    assert_eq!(
        fx.manager().state_get("global", "boots"),
        Some(Value::Int(1))
    );

    // Grant `command.restart` to `resource.controller`.
    fx.server
        .execute("add_ace resource.controller command.restart allow");

    let ok_canonical = fx
        .manager()
        .call_export(
            "controller",
            "restartTarget",
            &[Value::from("target")],
            None,
        )
        .expect("call export");
    assert_eq!(ok_canonical, vec![Value::Bool(true)]);
    assert_eq!(
        fx.manager().state_get("global", "boots"),
        Some(Value::Int(2))
    );

    let ok_alias = fx
        .manager()
        .call_export(
            "controller",
            "restartTarget",
            &[Value::from("target-alias")],
            None,
        )
        .expect("call export");
    assert_eq!(ok_alias, vec![Value::Bool(true)]);
    assert_eq!(
        fx.manager().state_get("global", "boots"),
        Some(Value::Int(3))
    );

    let missing = fx
        .manager()
        .call_export(
            "controller",
            "restartTarget",
            &[Value::from("does-not-exist")],
            None,
        )
        .expect("call export");
    assert_eq!(missing, vec![Value::Bool(false)]);
}

#[test]
fn resource_commands_game_type_and_instance_id_work_on_the_real_manager() {
    let fx = Fixture::new();
    fx.add_resource(
        "info",
        &manifest(&["server.lua"], &["commands", "setup", "instance"], &[]),
        &[(
            "server.lua",
            "RegisterCommand('alpha', function() end, false)\n\
             exports('commands', function() return GetResourceCommands('info') end)\n\
             exports('setup', function()\n\
                 SetGameType('race')\n\
                 SetMapName('m')\n\
             end)\n\
             exports('instance', function() return GetInstanceId() end)\n",
        )],
    );

    fx.manager().start("info").expect("info starts");
    assert_eq!(
        fx.manager()
            .call_export("info", "commands", &[], None)
            .expect("call export"),
        vec![Value::Array(vec![Value::from("alpha")])]
    );

    fx.manager()
        .call_export("info", "setup", &[], None)
        .expect("call export");
    assert_eq!(fx.manager().get_convar("gametype"), Some("race".to_owned()));
    assert_eq!(fx.manager().get_convar("mapname"), Some("m".to_owned()));

    assert_eq!(
        fx.manager()
            .call_export("info", "instance", &[], None)
            .expect("call export"),
        vec![Value::Int(i64::from(std::process::id()))]
    );
}

#[test]
fn refresh_fires_on_resource_list_refresh() {
    let fx = Fixture::new();
    add_observer(&fx);
    fx.add_resource(
        "listener",
        &manifest(&["server.lua"], &[], &["observer"]),
        &[(
            "server.lua",
            "AddEventHandler('onResourceListRefresh', function()\n\
                 exports.observer:record('refreshed')\n\
             end)\n",
        )],
    );
    fx.manager().start("observer").expect("observer starts");
    fx.manager().start("listener").expect("listener starts");

    fx.manager().refresh();
    fx.manager().refresh();

    let values = fx
        .manager()
        .call_export("observer", "log", &[], None)
        .expect("log");
    assert_eq!(values, vec![Value::from("refreshed refreshed")]);
}

/// Writes resource `name` in folder `folder` that depends on `dependencies`.
/// It records its name with the observer when it loads, and `stop name` when
/// it stops.
fn add_recorder(fx: &Fixture, folder: &str, name: &str, dependencies: &[&str]) {
    fx.add_resource(
        folder,
        &manifest(&["server.lua"], &[], dependencies),
        &[(
            "server.lua",
            &format!(
                "exports.observer:record('{name}')\n\
                 AddEventHandler('onResourceStop', function(stopped)\n\
                     if stopped == GetCurrentResourceName() then\n\
                         exports.observer:record('stop {name}')\n\
                     end\n\
                 end)\n"
            ),
        )],
    );
}

/// The observer's log, as one string.
fn observer_log(fx: &Fixture) -> String {
    match fx
        .manager()
        .call_export("observer", "log", &[], None)
        .expect("log")
        .first()
    {
        Some(Value::String(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("log returned {other:?}"),
    }
}

#[test]
fn start_category_starts_members_and_dependencies_in_order() {
    let fx = Fixture::new();
    add_observer(&fx);
    add_recorder(&fx, "[g]/a", "a", &["base"]);
    add_recorder(&fx, "[g]/b", "b", &["a"]);
    add_recorder(&fx, "base", "base", &[]);
    fx.manager().start("observer").expect("observer starts");

    fx.server.execute("start [g]");

    assert_eq!(observer_log(&fx), "base a b");
    assert!(fx.manager().is_running("base"));
    assert!(fx.manager().is_running("a"));
    assert!(fx.manager().is_running("b"));
}

#[test]
fn stop_category_stops_dependents_first_and_leaves_outside_dependencies() {
    let fx = Fixture::new();
    add_observer(&fx);
    add_recorder(&fx, "[g]/a", "a", &["base"]);
    add_recorder(&fx, "[g]/b", "b", &["a"]);
    add_recorder(&fx, "base", "base", &[]);
    fx.manager().start("observer").expect("observer starts");
    fx.server.execute("start [g]");

    fx.server.execute("stop [g]");

    assert_eq!(observer_log(&fx), "base a b stop b stop a");
    assert!(fx.manager().is_running("base"), "outside dependency stays");
    assert!(!fx.manager().is_running("a"));
    assert!(!fx.manager().is_running("b"));
}

#[test]
fn ensure_and_restart_category_restart_running_and_start_stopped() {
    let fx = Fixture::new();
    add_observer(&fx);
    add_recorder(&fx, "[g]/a", "a", &["base"]);
    add_recorder(&fx, "[g]/b", "b", &["a"]);
    add_recorder(&fx, "base", "base", &[]);
    fx.manager().start("observer").expect("observer starts");
    fx.manager().start("a").expect("a starts");
    assert_eq!(observer_log(&fx), "base a");

    fx.server.execute("ensure [g]");

    assert_eq!(observer_log(&fx), "base a stop a a b");
    assert!(fx.manager().is_running("a"));
    assert!(fx.manager().is_running("b"));

    fx.server.execute("restart [g]");

    assert_eq!(
        observer_log(&fx),
        "base a stop a a b stop b stop a a b stop b b"
    );
    assert!(fx.manager().is_running("a"));
    assert!(fx.manager().is_running("b"));
}

#[test]
fn an_unknown_category_changes_nothing() {
    let fx = Fixture::new();
    add_observer(&fx);
    add_recorder(&fx, "base", "base", &[]);
    fx.manager().start("observer").expect("observer starts");

    fx.server.execute("start [nope]");

    assert_eq!(observer_log(&fx), "");
    assert!(!fx.manager().is_running("base"));
    let error = fx
        .manager()
        .start_category("[nope]")
        .expect_err("unknown category");
    assert_eq!(error.to_string(), "no resource category `[nope]`");
}
