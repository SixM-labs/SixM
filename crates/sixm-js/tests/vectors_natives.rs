mod common;

use common::{call, first, new_client_host, new_host, new_host_with_convars, start};
use sixm_core::{ConvarKind, Value, joaat};

#[test]
fn vectors_support_properties_indexing_destructuring_and_roundtrip() {
    let host = new_host();
    let rt = start(
        &host,
        "vec",
        &["check", "echo"],
        &[(
            "index.js",
            r#"
            exports("echo", (v) => v);
            exports("check", () => {
                const v2 = vec2(1, 2);
                const v3 = vector3(3, 4, 5);
                const v4 = vector4(6, 7, 8, 9);
                const [a, b, c] = v3;
                const swiz = v4.wx;
                v3.x = 30;
                return [
                    v2.x, v2[1], v2.length,
                    a, b, c, v3[0],
                    swiz.x, swiz.y,
                    v4.toString(),
                    GetHashKey("adder")
                ];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "check", &[]),
        vec![Value::Array(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
            Value::Int(5),
            Value::Int(30),
            Value::Int(9),
            Value::Int(6),
            Value::from("vector4(6.0, 7.0, 8.0, 9.0)"),
            Value::Int(i64::from(joaat(b"adder").cast_signed())),
        ])]
    );
    assert_eq!(
        call(&rt, "echo", &[Value::Vector3([1.0, 2.0, 3.0])]),
        vec![Value::Vector3([1.0, 2.0, 3.0])]
    );
}

#[test]
fn convars_commands_and_game_timer_work() {
    let host = new_host();
    let rt = start(
        &host,
        "natives",
        &["check"],
        &[(
            "index.js",
            r#"
            let cmdOut = "";
            RegisterCommand("greet", (src, args, raw) => {
                cmdOut = `${src}:${args.join("+")}:${raw}`;
            }, true);
            exports("check", () => {
                SetConvar("a", "hello");
                SetConvarReplicated("b", "42");
                SetConvarServerInfo("c", "true");
                ExecuteCommand("say hi");
                const t = GetGameTimer();
                return [
                    GetConvar("sv_maxclients", "0"),
                    GetConvarInt("sv_maxclients", 0),
                    GetConvarBool("missing", true),
                    t >= 0,
                    cmdOut
                ];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "check", &[]),
        vec![Value::Array(vec![
            Value::from("32"),
            Value::Int(32),
            Value::Bool(true),
            Value::Bool(true),
            Value::from(""),
        ])]
    );
    assert_eq!(
        *host.convar_sets.borrow(),
        vec![
            ("a".to_owned(), "hello".to_owned(), ConvarKind::Plain),
            ("b".to_owned(), "42".to_owned(), ConvarKind::Replicated),
            ("c".to_owned(), "true".to_owned(), ConvarKind::ServerInfo),
        ]
    );
    assert_eq!(*host.commands.borrow(), vec!["say hi".to_owned()]);
    let _ = first;
}

#[test]
fn js_quat_and_entity_quaternion_work() {
    let host = new_host();
    let rt = start(
        &host,
        "quat_js",
        &["check", "echo"],
        &[(
            "index.js",
            r#"
            exports("echo", (q) => q);
            exports("check", () => {
                const q = quat(1, 0, -0.5, 0.25);
                const [w, x, y, z] = q;
                const qVec = quat(0.5, vec3(1, 2, 3));
                qVec.w = 2;
                const veh = CreateVehicle("adder", 0, 0, 0, 90, true, false);
                const [ex, ey, ez, ew] = GetEntityQuaternion(veh);
                SetEntityQuaternion(veh, 0, 0, 0, 1);
                return [
                    w, x, y, z,
                    q.length,
                    q.toString(),
                    JSON.stringify(q),
                    qVec[0], qVec.x, qVec.z,
                    Math.abs(ez - Math.SQRT1_2) < 1e-4 && Math.abs(ew - Math.SQRT1_2) < 1e-4,
                    GetEntityHeading(veh),
                    ex + ey
                ];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "echo", &[Value::Quat([1.0, 0.0, 0.5, -0.25])]),
        vec![Value::Quat([1.0, 0.0, 0.5, -0.25])]
    );
    assert_eq!(
        call(&rt, "check", &[]),
        vec![Value::Array(vec![
            Value::Int(1),
            Value::Int(0),
            Value::Float(-0.5),
            Value::Float(0.25),
            Value::Int(4),
            Value::from("quat(1.0, 0.0, -0.5, 0.25)"),
            Value::from(r#"{"w":1,"x":0,"y":-0.5,"z":0.25}"#),
            Value::Int(2),
            Value::Int(1),
            Value::Int(3),
            Value::Bool(true),
            Value::Int(0),
            Value::Int(0),
        ])]
    );
}

#[test]
fn get_convar_float_parses_trims_and_rejects_non_finite() {
    let host = new_host_with_convars(&[
        ("gravity", "  9.81 \n"),
        ("whole", "42"),
        ("nan_val", "NaN"),
        ("inf_val", "inf"),
        ("neg_inf", "-infinity"),
        ("bad", "nope"),
    ]);
    let rt = start(
        &host,
        "convar_float",
        &["check"],
        &[(
            "index.js",
            r#"
            exports("check", () => [
                GetConvarFloat("gravity", 1.0),
                GetConvarFloat("whole", 0.5),
                GetConvarFloat("missing", 3.25),
                GetConvarFloat("missing", 7),
                GetConvarFloat("nan_val", 1.5),
                GetConvarFloat("inf_val", 2.5),
                GetConvarFloat("neg_inf", -4.5),
                GetConvarFloat("bad", 9.5),
            ]);
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "check", &[]),
        vec![Value::Array(vec![
            Value::Float(9.81),
            Value::Int(42),
            Value::Float(3.25),
            Value::Int(7),
            Value::Float(1.5),
            Value::Float(2.5),
            Value::Float(-4.5),
            Value::Float(9.5),
        ])]
    );
}

#[test]
fn msgpack_round_trips_values_and_buffers() {
    let host = new_host();
    let rt = start(
        &host,
        "msgpack_srv",
        &["roundtrip"],
        &[(
            "index.js",
            r#"
            exports("roundtrip", () => {
                const payload = {
                    nil: null,
                    flag: true,
                    neg: -12,
                    big: 1000000,
                    flt: 3.25,
                    txt: "hello",
                    bin: new Uint8Array([0xff, 0x00, 0xfe]),
                    arr: [1, "two", false],
                    v2: vec2(1.5, -2.5),
                    v3: vector3(1, 2, 3),
                    v4: vector4(1, 2, 3, 4),
                    q: quat(1, 0, 0.5, -0.25),
                };
                const packed = msgpack.pack(payload);
                const fromUint8 = msgpack.unpack(packed);
                const fromBuf = msgpack.unpack(packed.buffer);
                const fromStr = msgpack.unpack("*");
                const emptyArg = msgpack.unpack(msgpack.pack());
                return [
                    packed instanceof Uint8Array,
                    fromUint8.nil === null,
                    fromUint8.flag,
                    fromUint8.neg,
                    fromUint8.big,
                    fromUint8.flt,
                    fromUint8.txt,
                    fromUint8.bin instanceof Uint8Array,
                    Array.from(fromUint8.bin).join(","),
                    fromUint8.arr[1],
                    fromUint8.v2.x, fromUint8.v2.y,
                    fromUint8.v3.x, fromUint8.v3.z,
                    fromUint8.v4.w,
                    fromUint8.q.y, fromUint8.q.z,
                    fromBuf.txt,
                    fromStr,
                    emptyArg === null,
                ];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "roundtrip", &[]),
        vec![Value::Array(vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Int(-12),
            Value::Int(1_000_000),
            Value::Float(3.25),
            Value::from("hello"),
            Value::Bool(true),
            Value::from("255,0,254"),
            Value::from("two"),
            Value::Float(1.5),
            Value::Float(-2.5),
            Value::Int(1),
            Value::Int(3),
            Value::Int(4),
            Value::Float(0.5),
            Value::Float(-0.25),
            Value::from("hello"),
            Value::Int(42),
            Value::Bool(true),
        ])]
    );
}

#[test]
fn msgpack_rejects_invalid_inputs_and_is_available_on_client() {
    let host = new_host();
    let rt = start(
        &host,
        "msgpack_err",
        &["errors"],
        &[(
            "index.js",
            r#"
            exports("errors", () => {
                const throws = (fn) => {
                    try { fn(); return false; } catch (e) { return true; }
                };
                let deep = 1;
                for (let i = 0; i < 40; i++) deep = [deep];
                return [
                    throws(() => msgpack.pack(() => {})),
                    throws(() => msgpack.pack({ f: () => {} })),
                    throws(() => msgpack.pack(new Map())),
                    throws(() => msgpack.pack(deep)),
                    throws(() => msgpack.unpack(123)),
                    throws(() => msgpack.unpack(null)),
                    throws(() => msgpack.unpack(new Uint8Array([]))),
                    throws(() => msgpack.unpack(new Uint8Array([0xc1]))),
                    throws(() => msgpack.unpack(new Uint8Array([0x01, 0x02]))),
                    throws(() => msgpack.unpack(new Uint8Array([0xdd, 0xff, 0xff, 0xff, 0xff]))),
                ];
            });
            "#,
        )],
    );

    assert_eq!(
        call(&rt, "errors", &[]),
        vec![Value::Array(vec![Value::Bool(true); 10])]
    );

    let cli_host = new_client_host(1);
    let cli = start(
        &cli_host,
        "msgpack_cli",
        &["check"],
        &[(
            "index.js",
            r#"exports("check", () => msgpack.unpack(msgpack.pack({ ok: 7 })).ok);"#,
        )],
    );
    assert_eq!(first(&cli, "check"), Value::Int(7));
}
