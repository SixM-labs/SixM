//! Vectors (`vector2`, `vector3`, `vector4`) and `GetHashKey` as scripts use them.

mod common;

use std::rc::Rc;

use common::{TestHost, call, first, new_client_host, new_host, start};
use sixm_core::{ClientTarget, GLOBAL_BAG, Host, Value};
use sixm_lua::LuaRuntime;

/// Runs `body` in a resource whose `run` export returns what `body` returns.
fn run(body: &str) -> Vec<Value> {
    let host = new_host();
    let script = format!("exports('run', function()\n{body}\nend)\n");
    let rt = start(&host, "vectors", &["run"], &[("main.lua", &script)]);
    call(&rt, "run", &[])
}

/// Runs `body` and returns its results as text. Strings are unquoted, and
/// other values use their debug form.
fn lines(body: &str) -> Vec<String> {
    run(body)
        .into_iter()
        .map(|value| match value {
            Value::String(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn constructors_and_aliases_make_vectors_of_their_size() {
    assert_eq!(
        lines(
            "return type(vector2(1, 2)), type(vec2(1, 2)), type(vector3(1, 2, 3)), \
             type(vec3(1, 2, 3)), type(vector4(1, 2, 3, 4)), type(vec4(1, 2, 3, 4))"
        ),
        [
            "vector2", "vector2", "vector3", "vector3", "vector4", "vector4"
        ]
    );
}

#[test]
fn numbers_and_vectors_are_flattened_in_order() {
    assert_eq!(
        run("local v = vector3(vector2(1, 2), 3); return v.x, v.y, v.z"),
        vec![Value::Float(1.0), Value::Float(2.0), Value::Float(3.0)]
    );
    assert_eq!(
        run("local v = vector4(vec2(1, 2), vector2(3, 4)); return v.w"),
        vec![Value::Float(4.0)]
    );
}

#[test]
fn a_single_number_fills_every_component() {
    assert_eq!(
        lines("return tostring(vector3(1)), tostring(vector4(-2)), tostring(vector2(0.5))"),
        [
            "vector3(1.0, 1.0, 1.0)",
            "vector4(-2.0, -2.0, -2.0, -2.0)",
            "vector2(0.5, 0.5)"
        ]
    );
}

#[test]
fn wrong_component_counts_are_errors() {
    let results = lines(
        "local ok, err = pcall(vector3, 1, 2)\n\
         local ok2, err2 = pcall(vector2, 1, 2, 3)\n\
         local ok3, err3 = pcall(vector4)\n\
         local ok4, err4 = pcall(vector3, 'a')\n\
         return tostring(ok), err, tostring(ok2), err2, tostring(ok3), err3, tostring(ok4), err4",
    );
    assert_eq!(results.len(), 8);
    assert_eq!(results[0], "false");
    assert!(
        results[1].contains("vector3: expected 3 components, got 2"),
        "{}",
        results[1]
    );
    assert_eq!(results[2], "false");
    assert!(
        results[3].contains("vector2: expected 2 components, got 3"),
        "{}",
        results[3]
    );
    assert_eq!(results[4], "false");
    assert!(
        results[5].contains("vector4: expected 4 components, got 0"),
        "{}",
        results[5]
    );
    assert_eq!(results[6], "false");
    assert!(
        results[7].contains("vector3: expected numbers or vectors, got string"),
        "{}",
        results[7]
    );
}

#[test]
fn fields_and_integer_keys_read_components() {
    assert_eq!(
        run("local v = vector3(4, 5, 6); return v.x, v.z, v[1], v[2], v[3]"),
        vec![
            Value::Float(4.0),
            Value::Float(6.0),
            Value::Float(4.0),
            Value::Float(5.0),
            Value::Float(6.0)
        ]
    );
    assert_eq!(
        run("local v = vector2(1, 2); return v.z, v.w, v[3], v[0], v.q"),
        vec![Value::Nil, Value::Nil, Value::Nil, Value::Nil, Value::Nil]
    );
}

#[test]
fn swizzles_return_vectors_of_their_length() {
    assert_eq!(
        lines(
            "local v = vector3(1, 2, 3)\n\
             local s = v.zyx\n\
             local t = vector4(1, 2, 3, 4).xxyy\n\
             local u = v.xy\n\
             return type(s), tostring(s), type(t), tostring(t), type(u), tostring(u)"
        ),
        [
            "vector3",
            "vector3(3.0, 2.0, 1.0)",
            "vector4",
            "vector4(1.0, 1.0, 2.0, 2.0)",
            "vector2",
            "vector2(1.0, 2.0)"
        ]
    );
    assert_eq!(
        run("local v = vector3(1, 2, 3); return v.w, v.xw, v.xyzw"),
        vec![Value::Nil, Value::Nil, Value::Nil],
        "a swizzle naming a component the vector lacks is nil"
    );
}

#[test]
fn assignment_changes_the_shared_vector() {
    assert_eq!(
        lines(
            "local v = vector3(0, 0, 0)\n\
             v.y = 7\n\
             v[3] = 9\n\
             local alias = v\n\
             alias.x = -1\n\
             return tostring(v), type(alias)"
        ),
        ["vector3(-1.0, 7.0, 9.0)", "vector3"]
    );
    assert_eq!(
        lines(
            "local v = vector2(1, 2)\n\
             local ok1 = pcall(function() v.z = 1 end)\n\
             local ok2 = pcall(function() v[3] = 1 end)\n\
             local ok3 = pcall(function() v.x = 'a' end)\n\
             local ok4 = pcall(function() v.q = 1 end)\n\
             return tostring(ok1), tostring(ok2), tostring(ok3), tostring(ok4), tostring(v)"
        ),
        ["false", "false", "false", "false", "vector2(1.0, 2.0)"]
    );
}

#[test]
fn arithmetic_works_component_wise_with_numbers_on_either_side() {
    assert_eq!(
        lines(
            "local a, b = vector3(1, 2, 3), vector3(4, 5, 6)\n\
             return tostring(a + b), tostring(b - a), tostring(a * b), tostring(b / a),\n\
               tostring(-a), tostring(a + 1), tostring(1 + a), tostring(a - 1),\n\
               tostring(10 - a), tostring(a * 2), tostring(2 * a), tostring(a / 2),\n\
               tostring(6 / a)"
        ),
        [
            "vector3(5.0, 7.0, 9.0)",
            "vector3(3.0, 3.0, 3.0)",
            "vector3(4.0, 10.0, 18.0)",
            "vector3(4.0, 2.5, 2.0)",
            "vector3(-1.0, -2.0, -3.0)",
            "vector3(2.0, 3.0, 4.0)",
            "vector3(2.0, 3.0, 4.0)",
            "vector3(0.0, 1.0, 2.0)",
            "vector3(9.0, 8.0, 7.0)",
            "vector3(2.0, 4.0, 6.0)",
            "vector3(2.0, 4.0, 6.0)",
            "vector3(0.5, 1.0, 1.5)",
            "vector3(6.0, 3.0, 2.0)"
        ]
    );
}

#[test]
fn vectors_of_different_sizes_do_not_combine() {
    let results = lines(
        "local ok, err = pcall(function() return vector3(1, 2, 3) + vector2(1, 2) end)\n\
         local ok2, err2 = pcall(function() return vector2(1, 2) * vector4(1) end)\n\
         return tostring(ok), err, tostring(ok2), err2",
    );
    assert_eq!(results[0], "false");
    assert!(
        results[1].contains("vector size mismatch: vector3 and vector2"),
        "{}",
        results[1]
    );
    assert_eq!(results[2], "false");
    assert!(
        results[3].contains("vector size mismatch: vector2 and vector4"),
        "{}",
        results[3]
    );
}

#[test]
fn length_is_the_euclidean_distance() {
    assert_eq!(
        run(
            "return #vector3(3, 4, 0), #(vector3(1, 2, 3) - vector3(1, 2, 3)), \
             #(vector2(6, 8) - vec2(0, 0))"
        ),
        vec![Value::Float(5.0), Value::Float(0.0), Value::Float(10.0)]
    );
}

#[test]
fn equality_needs_the_same_size_and_components() {
    assert_eq!(
        run(
            "return vector3(1, 2, 3) == vector3(1, 2, 3), vector3(1, 2, 3) == vector3(1, 2, 4),\n\
               vector2(1, 2) == vector3(1, 2, 0), vector3(1, 2, 3) == 1,\n\
               vector3(1, 2, 3) ~= vector3(1, 2, 4)"
        ),
        vec![
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true)
        ]
    );
}

#[test]
fn tostring_prints_whole_numbers_with_a_point() {
    assert_eq!(
        lines(
            "return tostring(vector3(1, 2.5, -3)), tostring(vector2(1, 2)),\n\
               tostring(vector4(1, 2, 3, 4)), tostring(vector2(0.1, 1e10))"
        ),
        [
            "vector3(1.0, 2.5, -3.0)",
            "vector2(1.0, 2.0)",
            "vector4(1.0, 2.0, 3.0, 4.0)",
            "vector2(0.1, 10000000000.0)"
        ]
    );
}

#[test]
fn type_names_vectors_and_keeps_other_answers() {
    assert_eq!(
        lines(
            "return type(vector2(1, 2)), type(vector3(1)), type(vector4(1)), type(1),\n\
               type('x'), type(nil), type({}), type(print), type(1.5), type(true),\n\
               type(coroutine.create(print))"
        ),
        [
            "vector2", "vector3", "vector4", "number", "string", "nil", "table", "function",
            "number", "boolean", "thread"
        ]
    );
    assert_eq!(
        run("return (pcall(type))"),
        vec![Value::Bool(false)],
        "type still rejects a call without a value"
    );
}

#[test]
fn a_vector_returned_by_an_export_arrives_as_a_vector() {
    let host = new_host();
    let _provider = start(
        &host,
        "provider",
        &["origin"],
        &[(
            "main.lua",
            "exports('origin', function() return vector3(1, 2, 3) end)\n",
        )],
    );
    let consumer = start(
        &host,
        "consumer",
        &["check", "pass"],
        &[(
            "main.lua",
            "exports('check', function()\n\
               local v = exports.provider:origin()\n\
               return type(v), v.x, v.y, v.z\n\
             end)\n\
             exports('pass', function() return exports.provider:origin() end)\n",
        )],
    );

    assert_eq!(
        call(&consumer, "check", &[]),
        vec![
            Value::from("vector3"),
            Value::Float(1.0),
            Value::Float(2.0),
            Value::Float(3.0)
        ]
    );
    assert_eq!(
        first(&consumer, "pass"),
        Value::Vector3([1.0, 2.0, 3.0]),
        "a vector passed back out of a resource stays a vector"
    );
}

#[test]
fn a_vector_argument_crosses_into_an_export() {
    let host = new_host();
    let provider = start(
        &host,
        "provider",
        &["double"],
        &[(
            "main.lua",
            "exports('double', function(v) return type(v) .. ' ' .. tostring(v * 2) end)\n",
        )],
    );
    assert_eq!(
        call(&provider, "double", &[Value::Vector2([1.5, -2.0])]),
        vec![Value::from("vector2 vector2(3.0, -4.0)")]
    );
}

#[test]
fn a_vector_in_a_state_bag_is_stored_as_a_vector() {
    let host = new_host();
    let rt = start(
        &host,
        "writer",
        &["run"],
        &[(
            "main.lua",
            "exports('run', function() GlobalState.pos = vector3(1, 2, 3) end)\n",
        )],
    );
    call(&rt, "run", &[]);
    assert_eq!(
        host.state_get(GLOBAL_BAG, "pos"),
        Some(Value::Vector3([1.0, 2.0, 3.0]))
    );
}

#[test]
fn server_and_client_events_carry_vectors() {
    let server = new_host();
    let rt = start(
        &server,
        "sender",
        &["send"],
        &[(
            "main.lua",
            "exports('send', function()\n\
               TriggerClientEvent('ev', 3, vector2(1, 2), vector4(0.5))\n\
             end)\n",
        )],
    );
    call(&rt, "send", &[]);
    assert_eq!(
        *server.client_events.borrow(),
        vec![(
            "ev".to_owned(),
            ClientTarget::Player(3),
            vec![
                Value::Vector2([1.0, 2.0]),
                Value::Vector4([0.5, 0.5, 0.5, 0.5])
            ]
        )]
    );

    let client: Rc<TestHost> = new_client_host(3);
    let rt = start(
        &client,
        "client",
        &["send"],
        &[(
            "main.lua",
            "exports('send', function() TriggerServerEvent('ev', vector3(1, 2, 3)) end)\n",
        )],
    );
    call(&rt, "send", &[]);
    assert_eq!(
        *client.server_events.borrow(),
        vec![("ev".to_owned(), vec![Value::Vector3([1.0, 2.0, 3.0])])]
    );
}

#[test]
fn a_vector_in_json_encodes_as_an_object_of_its_components() {
    assert_eq!(
        run("local t = json.decode(json.encode(vector3(1, 2, 3)))\n\
             return t.x == 1 and t.y == 2 and t.z == 3 and t.w == nil"),
        vec![Value::Bool(true)]
    );
    assert_eq!(
        lines("return json.encode(vector2(1, 2.5))"),
        [r#"{"x":1.0,"y":2.5}"#]
    );
    let results = lines(
        "local ok, err = pcall(json.encode, vector2(math.huge, 1))\nreturn tostring(ok), err",
    );
    assert_eq!(results[0], "false");
    assert!(
        results[1].contains("cannot encode NaN or infinity"),
        "{}",
        results[1]
    );
}

#[test]
fn get_hash_key_is_the_signed_joaat_of_the_lowercased_name() {
    assert_eq!(
        run("return GetHashKey('ADDER'), GetHashKey('adder'), GetHashKey('')"),
        vec![
            Value::Int(-1_216_765_807),
            Value::Int(-1_216_765_807),
            Value::Int(0)
        ]
    );
}

#[test]
fn get_hash_key_is_available_on_the_client() {
    let client = new_client_host(1);
    let rt: Rc<LuaRuntime> = start(
        &client,
        "client",
        &["hash"],
        &[(
            "main.lua",
            "exports('hash', function() return GetHashKey('adder') end)\n",
        )],
    );
    assert_eq!(first(&rt, "hash"), Value::Int(-1_216_765_807));
}

#[test]
fn lua_backtick_hash_literals_match_get_hash_key() {
    assert_eq!(
        run("local commented = '-- `sultan`' .. \"`adder`\"\n\
             local a = `adder`\n\
             local fn = assert(load('return `sultan`'))\n\
             return a == GetHashKey('adder'), fn() == GetHashKey('sultan'), commented"),
        vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::from("-- `sultan``adder`"),
        ]
    );
}

#[test]
fn quat_supports_arithmetic_rotation_json_and_cross_resource_values() {
    assert_eq!(
        run("local q = quat(1, 0, 0, 0)\n\
             local q2 = quat(0, vector3(0, 0, 1))\n\
             local rot = q2 * vector3(1, 0, 0)\n\
             local decoded = json.decode(json.encode(quat(0.5, -0.5, 0.25, -0.25)))\n\
             return type(q), tostring(q), #q, (q * q2).z, rot.x, decoded.w, decoded.x"),
        vec![
            Value::from("quat"),
            Value::from("quat(1.0, 0.0, 0.0, 0.0)"),
            Value::Float(1.0),
            Value::Float(1.0),
            Value::Float(-1.0),
            Value::Float(0.5),
            Value::Float(-0.5),
        ]
    );

    let host = new_host();
    let rt = start(
        &host,
        "quat_test",
        &["echo", "entity_quat"],
        &[(
            "main.lua",
            "exports('echo', function(q) return q end)\n\
             exports('entity_quat', function()\n\
               local v = CreateVehicle(`adder`, 0, 0, 0, 90.0, true, false)\n\
               local x, y, z, w = GetEntityQuaternion(v)\n\
               SetEntityQuaternion(v, 0.0, 0.0, 0.0, 1.0)\n\
               return x, y, z, w, GetEntityHeading(v), GetEntityScript(v)\n\
             end)\n",
        )],
    );
    assert_eq!(
        call(&rt, "echo", &[Value::Quat([1.0, 0.0, -0.5, 0.25])]),
        vec![Value::Quat([1.0, 0.0, -0.5, 0.25])]
    );
    let res = call(&rt, "entity_quat", &[]);
    assert!(
        matches!(res[2], Value::Float(z) if (z - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-4)
    );
    assert!(
        matches!(res[3], Value::Float(w) if (w - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-4)
    );
    assert_eq!(res[4], Value::Float(0.0));
    assert_eq!(res[5], Value::Nil); // TestHost uses default EntityStoreless Host::entity_script
}
