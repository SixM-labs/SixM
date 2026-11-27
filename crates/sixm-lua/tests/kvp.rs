//! Resource key-value functions: typed values, isolation between resources,
//! prefix search, limits, and persistence across a restart.

mod common;

use common::{new_host, start, text};

#[test]
fn typed_values_round_trip_and_missing_keys_have_defaults() {
    let host = new_host();
    let runtime = start(
        &host,
        "typed",
        &["run"],
        &[(
            "main.lua",
            "exports('run', function()\n\
               SetResourceKvp('name', 'sixm')\n\
               SetResourceKvpInt('count', 42)\n\
               SetResourceKvpFloat('ratio', 1.5)\n\
               return GetResourceKvpString('name') .. ':' .. GetResourceKvpInt('count')\n\
                 .. ':' .. GetResourceKvpFloat('ratio') .. ':'\n\
                 .. tostring(GetResourceKvpString('missing')) .. ':'\n\
                 .. GetResourceKvpInt('missing') .. ':' .. GetResourceKvpFloat('missing')\n\
             end)\n",
        )],
    );
    assert_eq!(text(&runtime, "run"), "sixm:42:1.5:nil:0:0.0");
}

#[test]
fn a_float_is_read_back_exactly() {
    let host = new_host();
    let runtime = start(
        &host,
        "floats",
        &["run"],
        &[(
            "main.lua",
            "exports('run', function()\n\
               SetResourceKvpFloat('third', 1 / 3)\n\
               return tostring(GetResourceKvpFloat('third') == 1 / 3)\n\
             end)\n",
        )],
    );
    assert_eq!(text(&runtime, "run"), "true");
}

#[test]
fn each_resource_sees_only_its_own_keys() {
    let host = new_host();
    let writer = start(
        &host,
        "writer",
        &["write"],
        &[(
            "main.lua",
            "exports('write', function() return tostring(SetResourceKvp('shared', 'mine')) end)\n",
        )],
    );
    let reader = start(
        &host,
        "reader",
        &["read"],
        &[(
            "main.lua",
            "exports('read', function() return tostring(GetResourceKvpString('shared')) end)\n",
        )],
    );
    assert_eq!(text(&writer, "write"), "true");
    assert_eq!(text(&reader, "read"), "nil");
}

#[test]
fn find_returns_the_keys_with_a_prefix_in_order_and_then_nil() {
    let host = new_host();
    let runtime = start(
        &host,
        "finder",
        &["run"],
        &[(
            "main.lua",
            "SetResourceKvp('p:b', '1')\n\
             SetResourceKvp('p:a', '1')\n\
             SetResourceKvp('q', '1')\n\
             exports('run', function()\n\
               local handle = StartFindKvp('p:')\n\
               local found = {}\n\
               while true do\n\
                 local key = FindKvp(handle)\n\
                 if not key then break end\n\
                 found[#found + 1] = key\n\
               end\n\
               local after_end = FindKvp(handle)\n\
               EndFindKvp(handle)\n\
               return table.concat(found, ',') .. '|' .. tostring(after_end)\n\
             end)\n",
        )],
    );
    assert_eq!(text(&runtime, "run"), "p:a,p:b|nil");
}

#[test]
fn limits_make_a_write_fail_without_storing_anything() {
    let host = new_host();
    let runtime = start(
        &host,
        "limits",
        &["run"],
        &[(
            "main.lua",
            "exports('run', function()\n\
               local ok_big = SetResourceKvp('big', string.rep('x', 65537))\n\
               local ok_max = SetResourceKvp('max', string.rep('x', 65536))\n\
               local ok_long = SetResourceKvp(string.rep('k', 257), 'v')\n\
               local ok_max_key = SetResourceKvp(string.rep('k', 256), 'v')\n\
               local ok_empty = SetResourceKvp('', 'v')\n\
               return tostring(ok_big) .. ',' .. tostring(ok_max) .. ',' .. tostring(ok_long)\n\
                 .. ',' .. tostring(ok_max_key) .. ',' .. tostring(ok_empty)\n\
                 .. ',' .. tostring(GetResourceKvpString('big'))\n\
             end)\n",
        )],
    );
    assert_eq!(text(&runtime, "run"), "false,true,false,true,false,nil");
}

#[test]
fn delete_reports_whether_the_key_was_set() {
    let host = new_host();
    let runtime = start(
        &host,
        "deleter",
        &["run"],
        &[(
            "main.lua",
            "exports('run', function()\n\
               SetResourceKvpNoSync('a', 'x')\n\
               local existed = DeleteResourceKvp('a')\n\
               local again = DeleteResourceKvpNoSync('a')\n\
               return tostring(existed) .. ',' .. tostring(again) .. ','\n\
                 .. tostring(GetResourceKvpString('a'))\n\
             end)\n",
        )],
    );
    assert_eq!(text(&runtime, "run"), "true,false,nil");
}

#[test]
fn values_survive_a_restart_once_flushed() {
    let dir = tempfile::tempdir().expect("temp dir");

    let first_run = new_host();
    first_run.set_kvp_dir(dir.path().to_path_buf());
    let writer = start(
        &first_run,
        "saved",
        &["write"],
        &[(
            "main.lua",
            "exports('write', function()\n\
               SetResourceKvp('owner', 'alice')\n\
               SetResourceKvpInt('level', 7)\n\
               SetResourceKvp('gone', 'x')\n\
               DeleteResourceKvp('gone')\n\
               FlushResourceKvp()\n\
               return 'ok'\n\
             end)\n",
        )],
    );
    assert_eq!(text(&writer, "write"), "ok");
    first_run.close_kvp();

    let second_run = new_host();
    second_run.set_kvp_dir(dir.path().to_path_buf());
    let reader = start(
        &second_run,
        "saved",
        &["read"],
        &[(
            "main.lua",
            "exports('read', function()\n\
               return GetResourceKvpString('owner') .. ':' .. GetResourceKvpInt('level')\n\
                 .. ':' .. tostring(GetResourceKvpString('gone'))\n\
             end)\n",
        )],
    );
    assert_eq!(text(&reader, "read"), "alice:7:nil");
}

#[test]
fn unflushed_values_are_not_on_disk_before_a_flush() {
    let dir = tempfile::tempdir().expect("temp dir");

    let first_run = new_host();
    first_run.set_kvp_dir(dir.path().to_path_buf());
    let writer = start(
        &first_run,
        "lazy",
        &["write"],
        &[(
            "main.lua",
            "exports('write', function() return tostring(SetResourceKvp('k', 'v')) end)\n",
        )],
    );
    assert_eq!(text(&writer, "write"), "true");

    let second_run = new_host();
    second_run.set_kvp_dir(dir.path().to_path_buf());
    let reader = start(
        &second_run,
        "lazy",
        &["read"],
        &[(
            "main.lua",
            "exports('read', function() return tostring(GetResourceKvpString('k')) end)\n",
        )],
    );
    assert_eq!(text(&reader, "read"), "nil");
}
