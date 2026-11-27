mod common;

use common::{call, first, new_host, start};
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
