//! The `msgpack` global: `msgpack.pack` and `msgpack.unpack`.

mod common;

use common::{call, first, new_client_host, new_host, start};
use sixm_core::{ScriptRuntime, Value};

#[test]
fn msgpack_is_available_on_both_server_and_client() {
    let server = new_host();
    let server_rt = start(
        &server,
        "srv",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function() return type(msgpack.pack), type(msgpack.unpack) end)\n",
        )],
    );
    assert_eq!(
        call(&server_rt, "probe", &[]),
        vec![Value::from("function"), Value::from("function")]
    );

    let client = new_client_host(1);
    let client_rt = start(
        &client,
        "cli",
        &["probe"],
        &[(
            "main.lua",
            "exports('probe', function() return type(msgpack.pack), type(msgpack.unpack) end)\n",
        )],
    );
    assert_eq!(
        call(&client_rt, "probe", &[]),
        vec![Value::from("function"), Value::from("function")]
    );
}

#[test]
fn msgpack_round_trips_primitives_binary_tables_vectors_and_quats() {
    let host = new_host();
    let rt = start(
        &host,
        "packer",
        &["roundtrip", "roundtrip_nil", "roundtrip_vec"],
        &[(
            "main.lua",
            "exports('roundtrip', function(v)\n\
               local packed = msgpack.pack(v)\n\
               assert(type(packed) == 'string')\n\
               return msgpack.unpack(packed)\n\
             end)\n\
             exports('roundtrip_nil', function()\n\
               return msgpack.unpack(msgpack.pack(nil)) == nil\n\
             end)\n\
             exports('roundtrip_vec', function()\n\
               local input = {\n\
                 v2 = vector2(1.5, -2.5),\n\
                 v3 = vector3(1.0, 2.0, 3.0),\n\
                 v4 = vector4(1.0, 2.0, 3.0, 4.0),\n\
                 q = quat(0.5, -0.5, 0.25, -0.25),\n\
               }\n\
               local out = msgpack.unpack(msgpack.pack(input))\n\
               return out.v2 == input.v2 and out.v3 == input.v3 and out.v4 == input.v4 and out.q == input.q\n\
             end)\n",
        )],
    );

    assert_eq!(first(&rt, "roundtrip_nil"), Value::Bool(true));
    assert_eq!(first(&rt, "roundtrip_vec"), Value::Bool(true));

    let samples = [
        Value::Bool(true),
        Value::Bool(false),
        Value::Int(0),
        Value::Int(-42),
        Value::Int(i64::MAX),
        Value::Float(3.25),
        Value::from("hello msgpack"),
        Value::String(Box::from([0xff_u8, 0x00, 0xfe, 0x80])),
        Value::Array(Vec::new()),
        Value::Array(vec![Value::Int(1), Value::from("two"), Value::Bool(true)]),
        Value::Map(vec![(Value::from("key"), Value::Int(99))]),
    ];
    for sample in samples {
        assert_eq!(
            call(&rt, "roundtrip", std::slice::from_ref(&sample)),
            vec![sample]
        );
    }

    let mixed_map = Value::Map(vec![
        (Value::from("key"), Value::Int(99)),
        (Value::Int(10), Value::from("ten")),
    ]);
    let out = call(&rt, "roundtrip", &[mixed_map]);
    let [Value::Map(pairs)] = out.as_slice() else {
        panic!("expected a map, got {out:?}");
    };
    assert_eq!(pairs.len(), 2);
    assert!(pairs.contains(&(Value::from("key"), Value::Int(99))));
    assert!(pairs.contains(&(Value::Int(10), Value::from("ten"))));
}

#[test]
fn msgpack_rejects_functions_cycles_and_malformed_payloads() {
    let host = new_host();
    let rt = start(
        &host,
        "errors",
        &["pack_fn", "pack_nested_fn", "pack_cycle", "unpack_bad"],
        &[(
            "main.lua",
            "exports('pack_fn', function() return pcall(msgpack.pack, function() end) end)\n\
             exports('pack_nested_fn', function() return pcall(msgpack.pack, { cb = function() end }) end)\n\
             exports('pack_cycle', function()\n\
               local t = {}\n\
               t.self = t\n\
               return pcall(msgpack.pack, t)\n\
             end)\n\
             exports('unpack_bad', function(raw) return pcall(msgpack.unpack, raw) end)\n",
        )],
    );

    for name in ["pack_fn", "pack_nested_fn", "pack_cycle"] {
        let out = call(&rt, name, &[]);
        assert_eq!(out[0], Value::Bool(false), "{name}: {out:?}");
    }

    let valid = rt
        .call_export(
            "unpack_bad",
            &[Value::String(Box::from([0x01_u8, 0x02]))],
            None,
        )
        .expect("pcall");
    assert_eq!(valid[0], Value::Bool(false), "trailing bytes should fail");

    for bad in [
        Box::from([]) as Box<[u8]>,
        Box::from([0xc1_u8]),
        Box::from([0xdd_u8, 0xff, 0xff, 0xff, 0xff]),
        Box::from([0x81_u8, 0xc0, 0x01]),
    ] {
        let out = call(&rt, "unpack_bad", &[Value::String(bad)]);
        assert_eq!(out[0], Value::Bool(false), "{out:?}");
    }
}
