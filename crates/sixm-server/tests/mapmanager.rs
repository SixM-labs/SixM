//! End-to-end coverage for the clean-room mapmanager resource.

mod common;

use common::Fixture;
use sixm_core::{Host, Value};

const MAPMANAGER_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/server-data/resources/[system]/mapmanager/"
);

fn stock_mapmanager(fx: &Fixture) {
    let server = std::fs::read_to_string(format!("{MAPMANAGER_PATH}server.lua"))
        .expect("read stock mapmanager server.lua");
    let client = std::fs::read_to_string(format!("{MAPMANAGER_PATH}client.lua"))
        .expect("read stock mapmanager client.lua");
    let manifest_text = std::fs::read_to_string(format!("{MAPMANAGER_PATH}fxmanifest.lua"))
        .expect("read stock mapmanager fxmanifest.lua");
    fx.add_resource(
        "mapmanager",
        &manifest_text,
        &[("server.lua", &server), ("client.lua", &client)],
    );
}

fn add_typed(fx: &Fixture, name: &str, kind: &str, extras: &str) {
    let text = format!("fx_version 'cerulean'\ngame 'common'\nresource_type '{kind}'\n{extras}");
    fx.add_resource(name, &text, &[]);
}

fn call(fx: &Fixture, export: &str, args: &[Value]) -> Vec<Value> {
    fx.manager()
        .call_export("mapmanager", export, args, None)
        .unwrap_or_else(|err| panic!("mapmanager:{export}: {err}"))
}

fn text(value: &Value) -> &str {
    let Value::String(value) = value else {
        panic!("expected string, got {value:?}");
    };
    std::str::from_utf8(value).expect("UTF-8")
}

fn grant_mapmanager(fx: &Fixture) {
    fx.server
        .execute("add_ace resource.mapmanager command.start allow");
    fx.server
        .execute("add_ace resource.mapmanager command.stop allow");
    fx.server
        .execute("add_ace resource.mapmanager command.restart allow");
}

fn setup() -> Fixture {
    let fx = Fixture::new();
    stock_mapmanager(&fx);
    fx.manager().start("mapmanager").expect("mapmanager starts");
    fx
}

#[test]
fn direct_resource_metadata_visibility_and_wrong_type_checks() {
    let fx = Fixture::new();
    stock_mapmanager(&fx);
    add_typed(
        &fx,
        "named_map",
        "map",
        "name 'Example Map'\ngameTypes 'race'\ngameTypes 'arena'\nmap 'one.json'\nmap 'two.json'\n",
    );
    add_typed(&fx, "fallback_map", "map", "");
    add_typed(&fx, "race", "gametype", "name 'Race Mode'\n");
    fx.add_resource(
        "meta_probe",
        "fx_version 'cerulean'\ngame 'common'\nserver_script 'probe.lua'\nserver_export 'inspect'\n",
        &[(
            "probe.lua",
            "exports('inspect', function()\n\
                return table.concat({\n\
                    tostring(GetResourceMetadata('named_map', 'resource_type', 0)),\n\
                    tostring(GetResourceMetadata('named_map', 'name', 0)),\n\
                    tostring(GetNumResourceMetadata('named_map', 'gameTypes')),\n\
                    tostring(GetResourceMetadata('named_map', 'gameTypes', 0)),\n\
                    tostring(GetResourceMetadata('named_map', 'gameTypes', 1)),\n\
                    tostring(GetNumResourceMetadata('named_map', 'map')),\n\
                    tostring(GetResourceMetadata('named_map', 'map', 0)),\n\
                    tostring(GetResourceMetadata('named_map', 'map', 1)),\n\
                }, '|')\n\
            end)\n",
        )],
    );
    fx.manager().start("mapmanager").expect("mapmanager starts");
    fx.manager().start("meta_probe").expect("meta_probe starts");

    assert_eq!(
        fx.text("meta_probe", "inspect"),
        "map|Example Map|2|race|arena|2|one.json|two.json"
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["named_map".into(), "named_map".into()]
        ),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["race".into(), "race".into()]
        ),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["missing".into(), "fallback_map".into()]
        ),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["race".into(), "missing".into()]
        ),
        [Value::Bool(false)]
    );
}

#[test]
fn resource_metadata_and_map_enumeration_use_scalar_manifest_values() {
    let fx = Fixture::new();
    stock_mapmanager(&fx);
    add_typed(
        &fx,
        "named_map",
        "map",
        "name 'Example Map'\ngameTypes 'race'\ngameTypes 'arena'\nmap 'one.json'\nmap 'two.json'\n",
    );
    add_typed(&fx, "fallback_map", "map", "");
    add_typed(&fx, "race", "gametype", "name 'Race Mode'\n");
    add_typed(&fx, "arena", "gametype", "name 'Arena Mode'\n");
    add_typed(&fx, "ordinary", "standalone", "");
    fx.manager().start("mapmanager").expect("mapmanager starts");

    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["race".into(), "named_map".into()]
        ),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["arena".into(), "named_map".into()]
        ),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["unknown".into(), "named_map".into()]
        ),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["race".into(), "fallback_map".into()]
        ),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["ordinary".into(), "fallback_map".into()]
        ),
        [Value::Bool(false)]
    );

    let maps = call(&fx, "getMaps", &[]);
    let Value::Map(maps) = &maps[0] else {
        panic!("getMaps returns a map, got {maps:?}");
    };
    assert_eq!(maps.len(), 2);
    let map_value = |name: &str| {
        maps.iter()
            .find(|(key, _)| key == &Value::from(name))
            .map(|(_, value)| value)
            .expect("map is listed")
    };
    let Value::Map(named) = map_value("named_map") else {
        panic!("map record is a map");
    };
    let field = |map: &[(Value, Value)], name: &str| {
        map.iter()
            .find(|(key, _)| key == &Value::from(name))
            .map_or(Value::Nil, |(_, value)| value.clone())
    };
    assert_eq!(text(&field(named, "name")), "Example Map");
    assert_eq!(
        field(named, "gameTypes"),
        Value::Array(vec!["race".into(), "arena".into()])
    );
    let Value::Map(fallback) = map_value("fallback_map") else {
        panic!("fallback map record is a map");
    };
    assert_eq!(text(&field(fallback, "name")), "fallback_map");
    assert_eq!(field(fallback, "gameTypes"), Value::Nil);
}

#[test]
fn provide_aliases_are_rejected_as_non_canonical() {
    let fx = setup();
    add_typed(
        &fx,
        "real_map",
        "map",
        "name 'Real Map'\nprovide 'alias_map'\n",
    );
    add_typed(
        &fx,
        "real_mode",
        "gametype",
        "name 'Real Mode'\nprovide 'alias_mode'\n",
    );
    fx.manager().refresh();
    grant_mapmanager(&fx);

    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["alias_mode".into(), "real_map".into()]
        ),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["real_mode".into(), "alias_map".into()]
        ),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(
            &fx,
            "doesMapSupportGameType",
            &["real_mode".into(), "real_map".into()]
        ),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeMap", &["alias_map".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::Nil]);
    assert_eq!(fx.manager().resource_state("real_map").name(), "stopped");
    assert_eq!(
        call(&fx, "changeGameType", &["alias_mode".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::Nil]);
    assert_eq!(fx.manager().resource_state("real_mode").name(), "stopped");

    let maps = call(&fx, "getMaps", &[]);
    let Value::Map(maps) = &maps[0] else {
        panic!("getMaps returns a map");
    };
    assert_eq!(maps.len(), 1);
    assert_eq!(maps[0].0, Value::from("real_map"));
}

#[test]
fn exports_change_current_map_and_gametype_and_publish_server_info() {
    let fx = setup();
    add_typed(&fx, "map_a", "map", "name 'Map A'\ngameTypes 'race'\n");
    add_typed(&fx, "map_b", "map", "name 'Map B'\ngameTypes 'arena'\n");
    add_typed(
        &fx,
        "map_shared",
        "map",
        "name 'Shared Map'\ngameTypes 'race'\ngameTypes 'arena'\n",
    );
    add_typed(&fx, "race", "gametype", "name 'Race Mode'\n");
    add_typed(&fx, "arena", "gametype", "name 'Arena Mode'\n");
    fx.manager().refresh();
    grant_mapmanager(&fx);

    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::Nil]);
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::Nil]);
    assert_eq!(
        call(&fx, "changeMap", &["race".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["map_a".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["missing".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(&fx, "changeMap", &["map_a".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["race".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("map_a")]);
    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::from("race")]);
    assert_eq!(fx.manager().get_convar("mapname").as_deref(), Some("Map A"));
    assert_eq!(
        fx.manager().get_convar("gametype").as_deref(),
        Some("Race Mode")
    );
    assert_eq!(
        call(&fx, "changeMap", &["map_b".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["arena".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::Nil]);
    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::from("arena")]);
    assert_eq!(fx.manager().get_convar("mapname").as_deref(), Some(""));
    assert_eq!(
        fx.manager().get_convar("gametype").as_deref(),
        Some("Arena Mode")
    );
    assert_eq!(
        call(&fx, "changeMap", &["map_shared".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["race".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["race".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("map_shared")]);
    assert_eq!(fx.manager().resource_state("map_shared").name(), "started");
    assert_eq!(
        fx.manager().get_convar("mapname").as_deref(),
        Some("Shared Map")
    );
    assert_eq!(
        call(&fx, "changeMap", &["missing".into()]),
        [Value::Bool(false)]
    );
}

#[test]
fn ace_denial_leaves_current_state_unchanged_and_allows_later_change() {
    let fx = setup();
    add_typed(&fx, "first", "map", "name 'First'\n");
    add_typed(&fx, "second", "map", "name 'Second'\n");
    add_typed(&fx, "mode_one", "gametype", "name 'Mode One'\n");
    add_typed(&fx, "mode_two", "gametype", "name 'Mode Two'\n");
    fx.manager().refresh();
    assert_eq!(
        call(&fx, "changeMap", &["first".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["mode_one".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::Nil]);
    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::Nil]);
    grant_mapmanager(&fx);
    assert_eq!(
        call(&fx, "changeMap", &["first".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["mode_one".into()]),
        [Value::Bool(true)]
    );
    fx.server
        .execute("remove_ace resource.mapmanager command.start allow");
    assert_eq!(
        call(&fx, "changeMap", &["second".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["mode_two".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("first")]);
    assert_eq!(
        call(&fx, "getCurrentGameType", &[]),
        [Value::from("mode_one")]
    );
    assert_eq!(fx.manager().get_convar("mapname").as_deref(), Some("First"));
    assert_eq!(
        fx.manager().get_convar("gametype").as_deref(),
        Some("Mode One")
    );
    assert_eq!(fx.manager().resource_state("second").name(), "stopped");
    assert_eq!(fx.manager().resource_state("mode_two").name(), "stopped");

    grant_mapmanager(&fx);
    assert_eq!(
        call(&fx, "changeMap", &["second".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("second")]);
}

#[test]
fn failed_target_start_preserves_current_map_and_convar() {
    let fx = setup();
    add_typed(&fx, "good_map", "map", "name 'Good'\n");
    add_typed(&fx, "new_dependency", "standalone", "");
    fx.add_resource(
        "bad_map",
        "fx_version 'cerulean'\ngame 'common'\nresource_type 'map'\ndependency 'new_dependency'\nserver_script 'missing.lua'\n",
        &[],
    );
    fx.manager().refresh();
    grant_mapmanager(&fx);
    assert_eq!(
        call(&fx, "changeMap", &["good_map".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeMap", &["bad_map".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("good_map")]);
    assert_eq!(fx.manager().get_convar("mapname").as_deref(), Some("Good"));
    assert_eq!(fx.manager().resource_state("bad_map").name(), "stopped");
    assert_eq!(
        fx.manager().resource_state("new_dependency").name(),
        "stopped"
    );
}

#[test]
fn round_ended_restarts_gametype_without_stopping_independent_map() {
    let fx = setup();
    add_typed(&fx, "independent_map", "map", "");
    fx.add_resource(
        "observer",
        "fx_version 'cerulean'\ngame 'common'\nserver_script 'observer.lua'\nserver_export 'count'\n",
        &[("observer.lua", "local n = 0\nAddEventHandler('onResourceStart', function(name) if name == 'race' then n = n + 1 end end)\nexports('count', function() return n end)\n")],
    );
    fx.add_resource(
        "race",
        "fx_version 'cerulean'\ngame 'common'\nresource_type 'gametype'\nname 'Race'\n",
        &[],
    );
    fx.manager().refresh();
    grant_mapmanager(&fx);
    fx.manager().start("observer").expect("observer starts");
    assert_eq!(
        call(&fx, "changeMap", &["independent_map".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["race".into()]),
        [Value::Bool(true)]
    );
    let before = fx
        .manager()
        .call_export("observer", "count", &[], None)
        .expect("count");
    assert_eq!(call(&fx, "roundEnded", &[]), [Value::Bool(true)]);
    let after = fx
        .manager()
        .call_export("observer", "count", &[], None)
        .expect("count");
    assert_ne!(before, after);
    assert_eq!(
        call(&fx, "getCurrentMap", &[]),
        [Value::from("independent_map")]
    );
    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::from("race")]);
    assert_eq!(call(&fx, "roundEnded", &[]), [Value::Bool(true)]);
}

#[test]
fn rollback_keeps_the_old_selection_when_stopping_it_would_cascade_into_target() {
    let fx = setup();
    add_typed(&fx, "base_map", "map", "");
    fx.add_resource(
        "dependent_map",
        "fx_version 'cerulean'\ngame 'common'\nresource_type 'map'\ndependency 'base_map'\n",
        &[],
    );
    fx.manager().refresh();
    grant_mapmanager(&fx);
    assert_eq!(
        call(&fx, "changeMap", &["base_map".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeMap", &["dependent_map".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("base_map")]);
    assert_eq!(fx.manager().resource_state("base_map").name(), "started");
    assert_eq!(
        fx.manager().resource_state("dependent_map").name(),
        "stopped"
    );
}

#[test]
fn mapmanager_local_start_event_is_once_and_observer_sees_committed_state() {
    let fx = setup();
    add_typed(&fx, "events_map", "map", "");
    fx.add_resource(
        "listener",
        "fx_version 'cerulean'\ngame 'common'\nserver_script 'listener.lua'\nserver_export 'log'\n",
        &[("listener.lua", "local entries = {}\nAddEventHandler('onMapStart', function(name) entries[#entries + 1] = tostring(source) .. ':' .. name .. ':' .. tostring(exports.mapmanager:getCurrentMap()) end)\nexports('log', function() return table.concat(entries, ',') end)\n")],
    );
    fx.manager().refresh();
    grant_mapmanager(&fx);
    fx.manager().start("listener").expect("listener starts");

    assert_eq!(
        call(&fx, "changeMap", &["events_map".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeMap", &["events_map".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        fx.text("listener", "log"),
        ":events_map:events_map",
        "a local lifecycle listener sees committed selection and no duplicate event"
    );
    fx.server.execute("stop events_map");
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::Nil]);
    assert_eq!(fx.manager().get_convar("mapname").as_deref(), Some(""));
}

#[test]
fn denied_stop_reconciles_to_the_still_running_old_map() {
    let fx = setup();
    add_typed(&fx, "old_map", "map", "name 'Old'\n");
    add_typed(&fx, "new_map", "map", "name 'New'\n");
    fx.manager().refresh();
    grant_mapmanager(&fx);
    assert_eq!(
        call(&fx, "changeMap", &["old_map".into()]),
        [Value::Bool(true)]
    );
    fx.server
        .execute("remove_ace resource.mapmanager command.stop allow");
    assert_eq!(
        call(&fx, "changeMap", &["new_map".into()]),
        [Value::Bool(false)]
    );
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("old_map")]);
    assert_eq!(fx.manager().get_convar("mapname").as_deref(), Some("Old"));
    assert_eq!(fx.manager().resource_state("old_map").name(), "started");
    assert_eq!(fx.manager().resource_state("new_map").name(), "stopped");
}

#[test]
fn on_gametype_start_and_external_lifecycle_reconcile() {
    let fx = setup();
    add_typed(&fx, "ext_map", "map", "name 'External Map'\n");
    add_typed(&fx, "ext_mode", "gametype", "name 'External Mode'\n");
    fx.add_resource(
        "listener",
        "fx_version 'cerulean'\ngame 'common'\nserver_script 'listener.lua'\nserver_export 'log'\n",
        &[(
            "listener.lua",
            "local entries = {}\n\
            AddEventHandler('onGameTypeStart', function(name)\n\
                entries[#entries + 1] = 'gt:' .. tostring(source) .. ':' .. name .. ':' .. tostring(exports.mapmanager:getCurrentGameType())\n\
            end)\n\
            AddEventHandler('onMapStart', function(name)\n\
                entries[#entries + 1] = 'map:' .. tostring(source) .. ':' .. name .. ':' .. tostring(exports.mapmanager:getCurrentMap())\n\
            end)\n\
            exports('log', function() return table.concat(entries, ',') end)\n",
        )],
    );
    fx.manager().refresh();
    grant_mapmanager(&fx);
    fx.manager().start("listener").expect("listener starts");

    assert_eq!(
        call(&fx, "changeGameType", &["ext_mode".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeGameType", &["ext_mode".into()]),
        [Value::Bool(true)]
    );
    fx.server.execute("start ext_map");
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("ext_map")]);
    assert_eq!(
        fx.manager().get_convar("mapname").as_deref(),
        Some("External Map")
    );
    fx.server.execute("stop ext_mode");
    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::Nil]);
    assert_eq!(fx.manager().get_convar("gametype").as_deref(), Some(""));
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("ext_map")]);
    fx.server.execute("start ext_mode");
    assert_eq!(
        call(&fx, "getCurrentGameType", &[]),
        [Value::from("ext_mode")]
    );
    assert_eq!(
        fx.manager().get_convar("gametype").as_deref(),
        Some("External Mode")
    );
    assert_eq!(
        fx.text("listener", "log"),
        "gt::ext_mode:ext_mode,map::ext_map:ext_map,gt::ext_mode:ext_mode"
    );
}

#[test]
fn round_ended_returns_false_without_gametype_or_restart_permission() {
    let fx = setup();
    add_typed(&fx, "race", "gametype", "name 'Race'\n");
    fx.manager().refresh();
    grant_mapmanager(&fx);

    assert_eq!(call(&fx, "roundEnded", &[]), [Value::Bool(false)]);
    assert_eq!(
        call(&fx, "changeGameType", &["race".into()]),
        [Value::Bool(true)]
    );
    fx.server
        .execute("remove_ace resource.mapmanager command.restart allow");
    assert_eq!(call(&fx, "roundEnded", &[]), [Value::Bool(false)]);
    assert_eq!(call(&fx, "getCurrentGameType", &[]), [Value::from("race")]);
    assert_eq!(fx.manager().get_convar("gametype").as_deref(), Some("Race"));
}

#[test]
fn server_reconciles_already_running_typed_resources_to_first_canonical_name() {
    let fx = Fixture::new();
    stock_mapmanager(&fx);
    add_typed(&fx, "beta_map", "map", "name 'Beta Map'\n");
    add_typed(&fx, "alpha_map", "map", "name 'Alpha Map'\n");
    add_typed(&fx, "beta_mode", "gametype", "name 'Beta Mode'\n");
    add_typed(&fx, "alpha_mode", "gametype", "name 'Alpha Mode'\n");
    fx.manager().start("beta_map").expect("beta_map starts");
    fx.manager().start("alpha_map").expect("alpha_map starts");
    fx.manager().start("beta_mode").expect("beta_mode starts");
    fx.manager().start("alpha_mode").expect("alpha_mode starts");
    fx.manager().start("mapmanager").expect("mapmanager starts");

    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("alpha_map")]);
    assert_eq!(
        call(&fx, "getCurrentGameType", &[]),
        [Value::from("alpha_mode")]
    );
    assert_eq!(
        fx.manager().get_convar("mapname").as_deref(),
        Some("Alpha Map")
    );
    assert_eq!(
        fx.manager().get_convar("gametype").as_deref(),
        Some("Alpha Mode")
    );
    assert_eq!(fx.manager().resource_state("beta_map").name(), "started");
    assert_eq!(fx.manager().resource_state("beta_mode").name(), "started");
}

#[test]
fn round_ended_restarts_a_dependent_map_through_real_lifecycle() {
    let fx = setup();
    fx.add_resource(
        "race",
        "fx_version 'cerulean'\ngame 'common'\nresource_type 'gametype'\nname 'Race'\n",
        &[],
    );
    fx.add_resource(
        "race_map",
        "fx_version 'cerulean'\ngame 'common'\nresource_type 'map'\nname 'Race Map'\ndependency 'race'\n",
        &[],
    );
    fx.add_resource(
        "observer",
        "fx_version 'cerulean'\ngame 'common'\nserver_script 'observer.lua'\nserver_export 'log'\n",
        &[("observer.lua", "local rows = {}\nAddEventHandler('onResourceStart', function(name) if name == 'race_map' then rows[#rows + 1] = 'start' end end)\nAddEventHandler('onResourceStop', function(name) if name == 'race_map' then rows[#rows + 1] = 'stop' end end)\nexports('log', function() return table.concat(rows, ',') end)\n")],
    );
    fx.manager().refresh();
    grant_mapmanager(&fx);
    fx.manager().start("observer").expect("observer starts");
    assert_eq!(
        call(&fx, "changeGameType", &["race".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(
        call(&fx, "changeMap", &["race_map".into()]),
        [Value::Bool(true)]
    );
    assert_eq!(call(&fx, "roundEnded", &[]), [Value::Bool(true)]);
    assert_eq!(fx.manager().resource_state("race_map").name(), "started");
    assert_eq!(call(&fx, "getCurrentMap", &[]), [Value::from("race_map")]);
    assert_eq!(fx.text("observer", "log"), "start,stop,start");
}

mod client_loopback {
    use std::net::SocketAddr;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use sixm_client::{Client, ClientRuntime};
    use sixm_net::Identity;
    use sixm_server::Input;
    use tempfile::TempDir;

    const TIMEOUT: Duration = Duration::from_secs(20);
    const STEP: Duration = Duration::from_millis(5);

    struct Harness {
        fx: Fixture,
        input: Receiver<Input>,
        runtime: Rc<ClientRuntime>,
        runtime2: Rc<ClientRuntime>,
        _dir: TempDir,
        _dir2: TempDir,
    }

    fn add_loopback_providers(fx: &Fixture) {
        fx.add_resource(
            "probe",
            "fx_version 'cerulean'\ngame 'common'\nserver_script 'server.lua'\nserver_export 'log'\n",
            &[("server.lua", "local rows = {}\nRegisterNetEvent('maptest:report', function(text) rows[#rows + 1] = tostring(source) .. ': ' .. text end)\nexports('log', function() return table.concat(rows, '\\n') end)\n")],
        );
        fx.add_resource(
            "directive_provider",
            "fx_version 'cerulean'\ngame 'common'\ndependency 'mapmanager'\nclient_script 'client.lua'\n",
            &[(
                "client.lua",
                "local savedAdd\n\
                AddEventHandler('getMapDirectives', function(add)\n\
                    savedAdd = add\n\
                    local emptyOk = pcall(add, '', function() end)\n\
                    local badTypeOk = pcall(add, 'bad', 'not-callable')\n\
                    local numTypeOk = pcall(add, 'bad2', 123)\n\
                    add('probe', function(data, mapName)\n\
                        TriggerServerEvent('maptest:report', 'directive=' .. tostring(data.value) .. ':' .. mapName)\n\
                    end)\n\
                    add('multi', function(data, mapName)\n\
                        TriggerServerEvent('maptest:report', 'multi-lua-1=' .. tostring(data.value) .. ':' .. mapName)\n\
                    end)\n\
                    add('multi', function(data, mapName)\n\
                        TriggerServerEvent('maptest:report', 'multi-lua-2=' .. tostring(data.value) .. ':' .. mapName)\n\
                    end)\n\
                    add('nilcheck', function(data, mapName)\n\
                        TriggerServerEvent('maptest:report', 'nilcheck=' .. tostring(data == nil) .. ':' .. mapName)\n\
                    end)\n\
                    add('broken', function() error('expected handler failure') end)\n\
                    if not emptyOk and not badTypeOk and not numTypeOk then\n\
                        TriggerServerEvent('maptest:report', 'add-arg-guards=ok')\n\
                    end\n\
                end)\n\
                AddEventHandler('onClientMapStart', function(name)\n\
                    TriggerServerEvent('maptest:report', 'map-start=' .. name)\n\
                end)\n\
                AddEventHandler('onClientMapStop', function(name)\n\
                    TriggerServerEvent('maptest:report', 'map-stop=' .. name)\n\
                end)\n\
                AddEventHandler('onClientGameTypeStart', function(name)\n\
                    TriggerServerEvent('maptest:report', 'type-start=' .. name)\n\
                end)\n\
                AddEventHandler('onClientGameTypeStop', function(name)\n\
                    TriggerServerEvent('maptest:report', 'type-stop=' .. name)\n\
                end)\n\
                AddEventHandler('onMapStart', function(name)\n\
                    TriggerServerEvent('maptest:report', 'unexpected-server-event=onMapStart:' .. tostring(name))\n\
                end)\n\
                AddEventHandler('onGameTypeStart', function(name)\n\
                    TriggerServerEvent('maptest:report', 'unexpected-server-event=onGameTypeStart:' .. tostring(name))\n\
                end)\n\
                AddEventHandler('onClientResourceStart', function(name)\n\
                    if name == 'sample_map' and savedAdd then\n\
                        local lateOk, lateErr = pcall(savedAdd, 'late_abuse', function() end)\n\
                        if not lateOk and tostring(lateErr):find('during getMapDirectives', 1, true) then\n\
                            TriggerServerEvent('maptest:report', 'late-add-guard=ok')\n\
                        end\n\
                        savedAdd = nil\n\
                    end\n\
                end)\n",
            )],
        );
        fx.add_resource(
            "one_shot_provider",
            "fx_version 'cerulean'\ngame 'common'\ndependency 'mapmanager'\nclient_script 'oneshot.lua'\n",
            &[(
                "oneshot.lua",
                "local registered = false\n\
                AddEventHandler('getMapDirectives', function(add)\n\
                    if not registered then\n\
                        registered = true\n\
                        add('oneshot', function(data, mapName)\n\
                            TriggerServerEvent('maptest:report', 'oneshot=' .. tostring(data.value) .. ':' .. mapName)\n\
                        end)\n\
                    end\n\
                end)\n",
            )],
        );
        fx.add_resource(
            "js_provider",
            "fx_version 'cerulean'\ngame 'common'\ndependency 'mapmanager'\nclient_script 'client.js'\n",
            &[(
                "client.js",
                "on('getMapDirectives', (add) => {\n\
                    add('jsprobe', (data, mapName) => {\n\
                        TriggerServerEvent('maptest:report', 'js-directive=' + data.value + ':' + mapName);\n\
                    });\n\
                    add('multi', (data, mapName) => {\n\
                        TriggerServerEvent('maptest:report', 'multi-js=' + data.value + ':' + mapName);\n\
                    });\n\
                    add('jsnil', (data, mapName) => {\n\
                        TriggerServerEvent('maptest:report', 'js-nil=' + (data === null) + ':' + mapName);\n\
                    });\n\
                });\n",
            )],
        );
    }

    fn add_loopback_maps(fx: &Fixture) {
        fx.add_resource(
            "sample_map",
            "fx_version 'cerulean'\ngame 'common'\nresource_type 'map'\nname 'Sample Map'\ndependency 'mapmanager'\nmap 'directives.json'\nmap 'unlisted.json'\nmap 'bad_shape.json'\nmap 'bad_name.json'\nmap 'more.json'\nmap 'bad.json'\nmap '../outside.json'\nfiles { 'directives.json', 'bad_shape.json', 'bad_name.json', 'more.json', 'bad.json' }\n",
            &[
                (
                    "directives.json",
                    r#"{"directives":[{"name":"probe","data":{"value":"first"}},{"name":"multi","data":{"value":"ordered"}},{"name":"oneshot","data":{"value":"once"}},{"name":"nilcheck","data":null},{"name":"nilcheck"},{"name":"jsnil","data":null},{"name":"broken","data":{}},{"name":"unknown","data":{}},{"name":"jsprobe","data":{"value":"from-js"}},{"name":"probe","data":{"value":"second"}}]}"#,
                ),
                (
                    "unlisted.json",
                    r#"{"directives":[{"name":"probe","data":{"value":"unlisted"}}]}"#,
                ),
                ("bad_shape.json", r#"{"directives":{}}"#),
                ("bad_name.json", r#"{"directives":[{"name":"","data":{}}]}"#),
                (
                    "more.json",
                    r#"{"directives":[{"name":"probe","data":{"value":"third"}}]}"#,
                ),
                ("bad.json", "not valid JSON"),
            ],
        );
        fx.add_resource(
            "late_provider",
            "fx_version 'cerulean'\ngame 'common'\ndependency 'mapmanager'\nclient_script 'late.lua'\n",
            &[("late.lua", "AddEventHandler('getMapDirectives', function(add) add('late', function(data, mapName) TriggerServerEvent('maptest:report', 'late=' .. data.value .. ':' .. mapName) end) end)\n")],
        );
        fx.add_resource(
            "late_map",
            "fx_version 'cerulean'\ngame 'common'\nresource_type 'map'\ndependency 'mapmanager'\nmap 'late.json'\nfiles { 'late.json' }\n",
            &[("late.json", r#"{"directives":[{"name":"late","data":{"value":"next-map"}},{"name":"oneshot","data":{"value":"should-not-run"}}]}"#)],
        );
        fx.add_resource(
            "sample_mode",
            "fx_version 'cerulean'\ngame 'common'\nresource_type 'gametype'\ndependency 'mapmanager'\n",
            &[],
        );
    }

    impl Harness {
        fn new() -> Self {
            let fx = Fixture::new();
            stock_mapmanager(&fx);
            add_loopback_providers(&fx);
            add_loopback_maps(&fx);
            fx.server.execute("endpoint_add_udp 127.0.0.1:0");
            let (sender, input) = mpsc::channel();
            let addr = fx.server.start_network(sender).expect("the network starts");
            fx.manager().start("mapmanager").expect("mapmanager starts");
            fx.manager().start("probe").expect("probe starts");
            fx.manager()
                .start("directive_provider")
                .expect("provider starts");
            fx.manager()
                .start("one_shot_provider")
                .expect("one_shot_provider starts");
            fx.manager()
                .start("js_provider")
                .expect("JS provider starts");
            fx.manager().start("sample_map").expect("map starts");
            fx.manager().start("sample_mode").expect("gametype starts");

            let dir = tempfile::tempdir().expect("first client temp dir");
            let runtime = Self::connect(
                &fx,
                &input,
                addr,
                "first",
                &dir.path().join("cache"),
                &dir.path().join("data"),
            );
            let dir2 = tempfile::tempdir().expect("second client temp dir");
            let runtime2 = Self::connect(
                &fx,
                &input,
                addr,
                "second",
                &dir2.path().join("cache"),
                &dir2.path().join("data"),
            );
            Self {
                fx,
                input,
                runtime,
                runtime2,
                _dir: dir,
                _dir2: dir2,
            }
        }

        fn connect(
            fx: &Fixture,
            input: &Receiver<Input>,
            addr: SocketAddr,
            player_name: &'static str,
            cache: &Path,
            data: &Path,
        ) -> Rc<ClientRuntime> {
            let cache = cache.to_path_buf();
            let handle = thread::spawn(move || {
                Client::connect(
                    addr,
                    player_name,
                    None,
                    &Identity::Ephemeral,
                    cache,
                    TIMEOUT,
                )
            });
            let deadline = Instant::now() + TIMEOUT;
            while !handle.is_finished() {
                assert!(Instant::now() < deadline, "client handshake timed out");
                match input.recv_timeout(STEP) {
                    Ok(input) => fx.server.handle(input),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            let client = handle
                .join()
                .expect("client thread does not panic")
                .expect("client connects");
            let runtime = ClientRuntime::new(client, data);
            runtime.start_synced();
            runtime
        }

        fn pump_until(&self, done: impl Fn(&str) -> bool) -> String {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                while let Ok(input) = self.input.try_recv() {
                    self.fx.server.handle(input);
                }
                self.runtime.pump(STEP);
                self.runtime.tick(Instant::now());
                self.runtime2.pump(STEP);
                self.runtime2.tick(Instant::now());
                let rows = self.fx.text("probe", "log");
                if done(&rows) {
                    return rows;
                }
                assert!(
                    Instant::now() < deadline,
                    "client/server timed out; rows:\n{rows}"
                );
                thread::sleep(STEP);
            }
        }
    }

    fn assert_initial_directives(rows: &str) {
        assert!(!rows.contains("unexpected-server-event="), "{rows}");
        assert!(!rows.contains("directive=unlisted:"), "{rows}");
        assert_eq!(rows.matches("1: map-start=sample_map").count(), 1, "{rows}");
        assert_eq!(rows.matches("2: map-start=sample_map").count(), 1, "{rows}");
        assert_eq!(
            rows.matches("1: type-start=sample_mode").count(),
            1,
            "{rows}"
        );
        assert_eq!(
            rows.matches("2: type-start=sample_mode").count(),
            1,
            "{rows}"
        );
        for client_id in ["1", "2"] {
            let lines: Vec<_> = rows
                .lines()
                .filter(|line| line.starts_with(&format!("{client_id}: ")))
                .collect();
            let pos = |needle: &str| {
                lines
                    .iter()
                    .position(|line| line.contains(needle))
                    .unwrap_or_else(|| {
                        panic!("missing `{needle}` for client {client_id}: {lines:?}")
                    })
            };
            let map_start = pos("map-start=sample_map");
            let first = pos("directive=first:sample_map");
            let multi_lua_1 = pos("multi-lua-1=ordered:sample_map");
            let multi_lua_2 = pos("multi-lua-2=ordered:sample_map");
            let multi_js = pos("multi-js=ordered:sample_map");
            let js = pos("js-directive=from-js:sample_map");
            let second = pos("directive=second:sample_map");
            let third = pos("directive=third:sample_map");
            assert!(
                map_start < first
                    && first < multi_lua_1
                    && multi_lua_1 < multi_lua_2
                    && multi_lua_2 < multi_js
                    && multi_js < js
                    && js < second
                    && second < third,
                "directive order for client {client_id}: {lines:?}"
            );
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.contains("nilcheck=true:sample_map"))
                    .count(),
                2,
                "expected null and absent data to arrive as nil on client {client_id}: {lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("js-nil=true:sample_map")),
                "expected null data to arrive as null in JS on client {client_id}: {lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("oneshot=once:sample_map")),
                "expected oneshot directive on client {client_id}: {lines:?}"
            );
            assert!(
                lines.iter().any(|line| line.contains("add-arg-guards=ok")),
                "expected invalid add() calls rejected on client {client_id}: {lines:?}"
            );
            assert!(
                lines.iter().any(|line| line.contains("late-add-guard=ok")),
                "expected late add() call rejected on client {client_id}: {lines:?}"
            );
        }
    }

    fn assert_restart_order(rows: &str, stop_needle: &str, start_needle: &str) {
        for client_id in ["1", "2"] {
            let lines: Vec<_> = rows
                .lines()
                .filter(|line| line.starts_with(&format!("{client_id}: ")))
                .collect();
            let stop_pos = lines
                .iter()
                .position(|line| line.contains(stop_needle))
                .expect("stop event on restart");
            let second_start_pos = lines
                .iter()
                .rposition(|line| line.contains(start_needle))
                .expect("second start event on restart");
            assert!(stop_pos < second_start_pos, "{lines:?}");
        }
    }

    #[test]
    fn client_lifecycle_and_json_map_directive_reach_real_loopback() {
        let h = Harness::new();
        let rows = h.pump_until(|rows| {
            rows.contains("directive=first:sample_map")
                && rows.contains("directive=second:sample_map")
                && rows.contains("directive=third:sample_map")
                && rows.contains("js-directive=from-js:sample_map")
                && rows.contains("multi-js=ordered:sample_map")
                && rows.contains("js-nil=true:sample_map")
                && rows.matches("late-add-guard=ok").count() >= 2
                && rows.matches("map-start=sample_map").count() >= 2
                && rows.matches("type-start=sample_mode").count() >= 2
        });
        assert_initial_directives(&rows);
        assert!(!rows.contains("mapmanager"), "{rows}");
        assert_eq!(h.runtime.resource_state("sample_map").name(), "started");

        h.fx.server.execute("restart sample_map");
        let restarted_map = h.pump_until(|rows| {
            rows.matches("1: map-stop=sample_map").count() == 1
                && rows.matches("2: map-stop=sample_map").count() == 1
                && rows.matches("1: map-start=sample_map").count() == 2
                && rows.matches("2: map-start=sample_map").count() == 2
        });
        assert_restart_order(
            &restarted_map,
            "map-stop=sample_map",
            "map-start=sample_map",
        );

        h.fx.server.execute("restart sample_mode");
        let restarted_mode = h.pump_until(|rows| {
            rows.matches("1: type-stop=sample_mode").count() == 1
                && rows.matches("2: type-stop=sample_mode").count() == 1
                && rows.matches("1: type-start=sample_mode").count() == 2
                && rows.matches("2: type-start=sample_mode").count() == 2
        });
        assert_restart_order(
            &restarted_mode,
            "type-stop=sample_mode",
            "type-start=sample_mode",
        );

        h.fx.server.execute("stop sample_map");
        let stopped = h.pump_until(|rows| {
            rows.matches("1: map-stop=sample_map").count() == 2
                && rows.matches("2: map-stop=sample_map").count() == 2
        });
        assert_eq!(stopped.matches("1: map-stop=sample_map").count(), 2);
        assert_eq!(stopped.matches("2: map-stop=sample_map").count(), 2);
        h.fx.server.execute("stop sample_mode");
        let stopped = h.pump_until(|rows| {
            rows.matches("1: type-stop=sample_mode").count() == 2
                && rows.matches("2: type-stop=sample_mode").count() == 2
        });
        assert_eq!(stopped.matches("1: type-stop=sample_mode").count(), 2);
        assert_eq!(stopped.matches("2: type-stop=sample_mode").count(), 2);

        h.fx.manager()
            .start("late_provider")
            .expect("late provider starts");
        assert!(!h.fx.text("probe", "log").contains("late=next-map:late_map"));
        h.fx.manager().start("late_map").expect("later map starts");
        let rows = h.pump_until(|rows| {
            rows.matches("1: late=next-map:late_map").count() == 1
                && rows.matches("2: late=next-map:late_map").count() == 1
        });
        assert!(!rows.contains("oneshot=once:late_map"), "{rows}");
        assert!(!rows.contains("oneshot=should-not-run:late_map"), "{rows}");
        assert!(!rows.contains("unexpected-server-event="), "{rows}");
    }

    #[test]
    fn client_startup_reconciles_already_running_map_and_gametype() {
        let fx = Fixture::new();
        stock_mapmanager(&fx);
        fx.add_resource(
            "probe",
            "fx_version 'cerulean'\ngame 'common'\nserver_script 'server.lua'\nserver_export 'log'\n",
            &[("server.lua", "local rows = {}\nRegisterNetEvent('maptest:report', function(text) rows[#rows + 1] = tostring(source) .. ': ' .. text end)\nexports('log', function() return table.concat(rows, '\\n') end)\n")],
        );
        fx.add_resource(
            "observer",
            "fx_version 'cerulean'\ngame 'common'\nclient_script 'client.lua'\n",
            &[(
                "client.lua",
                "AddEventHandler('getMapDirectives', function(add)\n\
                    add('probe', function(data, mapName)\n\
                        TriggerServerEvent('maptest:report', 'directive=' .. tostring(data.value) .. ':' .. mapName)\n\
                    end)\n\
                end)\n\
                AddEventHandler('onClientMapStart', function(name)\n\
                    TriggerServerEvent('maptest:report', 'map-start=' .. name)\n\
                end)\n\
                AddEventHandler('onClientGameTypeStart', function(name)\n\
                    TriggerServerEvent('maptest:report', 'type-start=' .. name)\n\
                end)\n",
            )],
        );
        fx.add_resource(
            "already_map",
            "fx_version 'cerulean'\ngame 'common'\nresource_type 'map'\nmap 'd.json'\nfiles { 'd.json' }\n",
            &[("d.json", r#"{"directives":[{"name":"probe","data":{"value":"prestarted"}}]}"#)],
        );
        fx.add_resource(
            "already_mode",
            "fx_version 'cerulean'\ngame 'common'\nresource_type 'gametype'\n",
            &[],
        );

        fx.server.execute("endpoint_add_udp 127.0.0.1:0");
        let (sender, input) = mpsc::channel();
        let addr = fx.server.start_network(sender).expect("the network starts");
        fx.manager().start("probe").expect("probe starts");
        fx.manager().start("observer").expect("observer starts");
        fx.manager()
            .start("already_map")
            .expect("already_map starts");
        fx.manager()
            .start("already_mode")
            .expect("already_mode starts");
        fx.manager().start("mapmanager").expect("mapmanager starts");

        let dir = tempfile::tempdir().expect("first client temp dir");
        let runtime = Harness::connect(
            &fx,
            &input,
            addr,
            "first",
            &dir.path().join("cache"),
            &dir.path().join("data"),
        );
        let dir2 = tempfile::tempdir().expect("second client temp dir");
        let runtime2 = Harness::connect(
            &fx,
            &input,
            addr,
            "second",
            &dir2.path().join("cache"),
            &dir2.path().join("data"),
        );
        let h = Harness {
            fx,
            input,
            runtime,
            runtime2,
            _dir: dir,
            _dir2: dir2,
        };
        let rows = h.pump_until(|rows| {
            rows.matches("map-start=already_map").count() == 2
                && rows.matches("type-start=already_mode").count() == 2
                && rows.matches("directive=prestarted:already_map").count() == 2
        });
        assert!(rows.contains("1: map-start=already_map"), "{rows}");
        assert!(rows.contains("2: map-start=already_map"), "{rows}");
        assert!(rows.contains("1: type-start=already_mode"), "{rows}");
        assert!(rows.contains("2: type-start=already_mode"), "{rows}");
        assert!(
            rows.contains("1: directive=prestarted:already_map"),
            "{rows}"
        );
        assert!(
            rows.contains("2: directive=prestarted:already_map"),
            "{rows}"
        );
    }
}
