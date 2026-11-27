//! Resource key-value storage on the server: values outlive the server instance
//! that wrote them.

mod common;

use std::time::Instant;

use common::{Fixture, manifest};
use sixm_core::{Host, Value};
use sixm_server::Server;

const SCRIPT: &str = "\
exports('write', function()\n\
    SetResourceKvp('owner', 'alice')\n\
    SetResourceKvpInt('level', 7)\n\
    SetResourceKvpFloat('ratio', 0.25)\n\
    SetResourceKvp('player:1', 'a')\n\
    SetResourceKvp('player:2', 'b')\n\
    SetResourceKvp('gone', 'x')\n\
    DeleteResourceKvp('gone')\n\
    return 'ok'\n\
end)\n\
exports('read', function()\n\
    local found = {}\n\
    local handle = StartFindKvp('player:')\n\
    while true do\n\
        local key = FindKvp(handle)\n\
        if not key then break end\n\
        found[#found + 1] = key\n\
    end\n\
    EndFindKvp(handle)\n\
    return GetResourceKvpString('owner') .. ':' .. GetResourceKvpInt('level')\n\
        .. ':' .. GetResourceKvpFloat('ratio') .. ':' .. table.concat(found, ',')\n\
        .. ':' .. tostring(GetResourceKvpString('gone'))\n\
end)\n";

/// A new server instance on the resources folder of `first`.
fn restarted(first: &Fixture, kvp_path: &str) -> Server {
    let server = Server::new();
    server.execute(&format!(
        "set sixm_resourcesPath \"{}\"",
        first.resources().display()
    ));
    server.execute("set sixm_identityFile \"\"");
    server.execute(&format!("set sixm_kvpPath \"{kvp_path}\""));
    server
}

/// The first result of export `export` of `resource` on `server`, as text.
fn text(server: &Server, resource: &str, export: &str) -> String {
    let values = server
        .manager()
        .call_export(resource, export, &[], None)
        .unwrap_or_else(|err| panic!("export `{export}` of `{resource}`: {err}"));
    match values.first() {
        Some(Value::String(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("export `{export}` returned {other:?}, not a string"),
    }
}

#[test]
fn values_survive_a_server_restart() {
    let fx = Fixture::new();
    let kvp_dir = fx.root().join("kvp");
    let kvp_path = kvp_dir.display().to_string();
    fx.server
        .execute(&format!("set sixm_kvpPath \"{kvp_path}\""));
    fx.add_resource(
        "saved",
        &manifest(&["server.lua"], &["write", "read"], &[]),
        &[("server.lua", SCRIPT)],
    );
    fx.manager().start("saved").expect("saved starts");
    assert_eq!(text(&fx.server, "saved", "write"), "ok");
    // A frame hands the batch to the writer thread, and the shutdown waits for it.
    fx.server.tick(Instant::now());
    fx.server.shutdown();
    assert!(kvp_dir.join("saved.kvp").is_file());

    let again = restarted(&fx, &kvp_path);
    again.manager().start("saved").expect("saved starts again");
    assert_eq!(
        text(&again, "saved", "read"),
        "alice:7:0.25:player:1,player:2:nil"
    );
    again.shutdown();
}

#[test]
fn changes_made_after_a_restart_are_kept_too() {
    let fx = Fixture::new();
    let kvp_path = fx.root().join("kvp").display().to_string();
    fx.server
        .execute(&format!("set sixm_kvpPath \"{kvp_path}\""));
    fx.add_resource(
        "counter",
        &manifest(&["server.lua"], &["bump"], &[]),
        &[(
            "server.lua",
            "exports('bump', function()\n\
                 local n = GetResourceKvpInt('n') + 1\n\
                 SetResourceKvpInt('n', n)\n\
                 return tostring(n)\n\
             end)\n",
        )],
    );
    fx.manager().start("counter").expect("counter starts");
    assert_eq!(text(&fx.server, "counter", "bump"), "1");
    fx.server.shutdown();

    for expected in ["2", "3"] {
        let server = restarted(&fx, &kvp_path);
        server.manager().start("counter").expect("counter starts");
        assert_eq!(text(&server, "counter", "bump"), expected);
        server.shutdown();
    }
}
