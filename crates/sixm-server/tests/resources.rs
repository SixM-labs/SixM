//! Resource natives on the server: lifecycle states seen from scripts, the
//! sorted resource list and manifest metadata.

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
