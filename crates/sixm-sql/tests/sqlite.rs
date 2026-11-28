//! Unit and SQLite integration tests for `sixm-sql`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sixm_core::{FuncRef, Value};
use sixm_sql::config::{DatabaseConfig, SqlBackend, SqliteTarget, SslMode};
use sixm_sql::params::{Dialect, ParsedSql, bind_query};
use sixm_sql::{
    LimitKind, ParamSet, PrepareParams, QueryOutcome, QueryResultSet, SqlError, SqlLimits,
    SqlOperation, SqlPool, SqlValue, TransactionItem,
};

fn named(pairs: &[(&str, SqlValue)]) -> ParamSet {
    let mut map = BTreeMap::new();
    for (k, v) in pairs {
        map.insert((*k).to_owned(), v.clone());
    }
    ParamSet::Named(map)
}

#[test]
fn config_parses_empty_as_none() {
    assert_eq!(DatabaseConfig::parse("").expect("empty"), None);
    assert_eq!(DatabaseConfig::parse("   \t\n ").expect("whitespace"), None);
    assert!(
        SqlPool::from_connection_string("", SqlLimits::default())
            .expect("empty pool")
            .is_none()
    );
}

#[test]
fn config_parses_mysql_uris_and_percent_encoding_and_redacts_debug() {
    let uri = "mysql://admin:p%40ss%3Aw%2Ford%25@db.example.com:3307/roleplay_db?charset=utf8mb4&connectTimeout=4500&sslmode=verify_identity&dateStrings=true&multipleStatements=false";
    let parsed = DatabaseConfig::parse(uri)
        .expect("valid uri")
        .expect("some config");
    assert_eq!(parsed.backend(), SqlBackend::Mysql);

    let DatabaseConfig::Mysql(cfg) = &parsed else {
        panic!("expected mysql config");
    };
    assert_eq!(cfg.host, "db.example.com");
    assert_eq!(cfg.port, 3307);
    assert_eq!(cfg.user.as_deref(), Some("admin"));
    assert_eq!(cfg.password.as_deref(), Some("p@ss:w/ord%"));
    assert_eq!(cfg.database.as_deref(), Some("roleplay_db"));
    assert_eq!(cfg.charset, "utf8mb4");
    assert_eq!(cfg.connect_timeout, Some(Duration::from_millis(4500)));
    assert_eq!(cfg.ssl_mode, SslMode::VerifyIdentity);
    assert!(cfg.date_strings);

    let debug_str = format!("{parsed:?}");
    assert!(debug_str.contains("[REDACTED]"));
    assert!(!debug_str.contains("p@ss:w/ord%"));

    // Aliases and IPv6
    let ipv6 = DatabaseConfig::parse("mariadb://root:pw@[::1]:3308/test")
        .expect("ipv6")
        .expect("some");
    let DatabaseConfig::Mysql(ipv6_cfg) = ipv6 else {
        panic!("expected mysql");
    };
    assert_eq!(ipv6_cfg.host, "::1");
    assert_eq!(ipv6_cfg.port, 3308);
    assert_eq!(ipv6_cfg.database.as_deref(), Some("test"));

    let mysql2 = DatabaseConfig::parse("mysql2://localhost/esx")
        .expect("mysql2")
        .expect("some");
    assert_eq!(mysql2.backend(), SqlBackend::Mysql);
}

#[test]
fn config_parses_semicolon_key_value_and_sqlite_forms() {
    let kv = "Server=127.0.0.1;Port=3309;Uid=fivem;Pwd=super_secret;Database=qbcore;Charset=utf8mb4;SslMode=require;ConnectTimeout=3000;";
    let parsed = DatabaseConfig::parse(kv).expect("kv").expect("some");
    let DatabaseConfig::Mysql(cfg) = &parsed else {
        panic!("expected mysql");
    };
    assert_eq!(cfg.host, "127.0.0.1");
    assert_eq!(cfg.port, 3309);
    assert_eq!(cfg.user.as_deref(), Some("fivem"));
    assert_eq!(cfg.password.as_deref(), Some("super_secret"));
    assert_eq!(cfg.database.as_deref(), Some("qbcore"));
    assert_eq!(cfg.ssl_mode, SslMode::Required);
    assert_eq!(cfg.connect_timeout, Some(Duration::from_millis(3000)));
    assert!(!format!("{parsed:?}").contains("super_secret"));

    assert_eq!(
        DatabaseConfig::parse("sqlite::memory:").expect("mem"),
        Some(DatabaseConfig::Sqlite(sixm_sql::SqliteConfig {
            target: SqliteTarget::Memory
        }))
    );
    assert_eq!(
        DatabaseConfig::parse("sqlite://:memory:").expect("mem2"),
        Some(DatabaseConfig::Sqlite(sixm_sql::SqliteConfig {
            target: SqliteTarget::Memory
        }))
    );
    assert_eq!(
        DatabaseConfig::parse("driver=sqlite;filename=data/server.db").expect("kv sqlite"),
        Some(DatabaseConfig::Sqlite(sixm_sql::SqliteConfig {
            target: SqliteTarget::File("data/server.db".into())
        }))
    );
    assert_eq!(
        DatabaseConfig::parse("sqlite:local.db").expect("sqlite:"),
        Some(DatabaseConfig::Sqlite(sixm_sql::SqliteConfig {
            target: SqliteTarget::File("local.db".into())
        }))
    );
}

#[test]
fn config_rejects_multiple_statements_and_invalid_inputs() {
    let uri_err = DatabaseConfig::parse("mysql://root@127.0.0.1/db?multipleStatements=true")
        .expect_err("must reject multipleStatements");
    assert!(matches!(uri_err, SqlError::InvalidConfig(_)));

    let kv_err = DatabaseConfig::parse("host=127.0.0.1;database=db;multipleStatements=true")
        .expect_err("must reject multipleStatements in kv");
    assert!(matches!(kv_err, SqlError::InvalidConfig(_)));

    assert!(matches!(
        DatabaseConfig::parse("postgres://localhost/db"),
        Err(SqlError::InvalidConfig(_))
    ));
    assert!(matches!(
        DatabaseConfig::parse("mysql://127.0.0.1:0/db"),
        Err(SqlError::InvalidConfig(_))
    ));
    assert!(matches!(
        DatabaseConfig::parse("mysql://user:bad%ZZ@127.0.0.1/db"),
        Err(SqlError::InvalidConfig(_))
    ));
}

#[test]
fn params_ignore_literals_identifiers_and_comments_and_rewrite_named() {
    let mysql_sql = "
        -- comment with ? and @ignored and ;
        /* block ? :ignored ; */
        SELECT 'it\\'s ? and @not_a_param', 'escaped '' ? '' quote', \"col?\", `tbl@1`, @@version
        FROM users
        WHERE id = @id AND status = :status OR backup_id = @id;
    ";
    let params = named(&[
        ("id", SqlValue::Int(42)),
        ("status", SqlValue::Text("active".to_owned())),
        ("extra_unused", SqlValue::Bool(true)),
    ]);

    let mysql_bound = bind_query(mysql_sql, &params, Dialect::Mysql).expect("mysql bind");
    assert!(!mysql_bound.sql.ends_with(';'));
    assert_eq!(
        mysql_bound.params,
        vec![
            SqlValue::Int(42),
            SqlValue::Text("active".to_owned()),
            SqlValue::Int(42),
        ]
    );
    assert!(
        mysql_bound
            .sql
            .contains("id = ? AND status = ? OR backup_id = ?")
    );
    assert!(mysql_bound.sql.contains("@@version"));

    let sqlite_sql = "
        -- comment with ? and @ignored and ;
        /* block ? :ignored ; */
        SELECT 'C:\\path\\', 'escaped '' ? and @not_a_param '' quote', \"col?\", `tbl@1`
        FROM users
        WHERE id = @id AND status = :status OR backup_id = @id;
    ";
    let sqlite_bound = bind_query(sqlite_sql, &params, Dialect::Sqlite).expect("sqlite bind");
    assert!(
        sqlite_bound
            .sql
            .contains("id = ?1 AND status = ?2 OR backup_id = ?1")
    );
    assert_eq!(
        sqlite_bound.params,
        vec![SqlValue::Int(42), SqlValue::Text("active".to_owned())]
    );
}

#[test]
fn params_reject_mismatches_stacked_queries_and_invalid_core_types() {
    assert!(matches!(
        ParsedSql::parse("SELECT 1; DROP TABLE users"),
        Err(SqlError::InvalidSql(_))
    ));
    assert!(matches!(
        ParsedSql::parse("SELECT 'unterminated"),
        Err(SqlError::InvalidSql(_))
    ));
    assert!(matches!(
        ParsedSql::parse("SELECT /* unterminated"),
        Err(SqlError::InvalidSql(_))
    ));
    assert!(matches!(
        ParsedSql::parse("SELECT * FROM t WHERE a = ? AND b = @b"),
        Err(SqlError::InvalidParam(_))
    ));
    assert!(matches!(
        bind_query(
            "SELECT * FROM t WHERE a = ? AND b = ?",
            &ParamSet::Positional(vec![SqlValue::Int(1)]),
            Dialect::Sqlite
        ),
        Err(SqlError::InvalidParam(_))
    ));
    assert!(matches!(
        bind_query(
            "SELECT * FROM t WHERE a = @missing",
            &named(&[("other", SqlValue::Int(1))]),
            Dialect::Sqlite
        ),
        Err(SqlError::InvalidParam(_))
    ));

    // Reject function references, nested tables, and non-finite floats.
    let func = Value::FuncRef(FuncRef {
        owner: Arc::from("res"),
        instance: 1,
        id: 7,
    });
    assert!(matches!(
        ParamSet::from_core_value(&Value::Array(vec![func])),
        Err(SqlError::InvalidParam(_))
    ));
    assert!(matches!(
        ParamSet::from_core_value(&Value::Array(vec![Value::Array(vec![Value::Int(1)])])),
        Err(SqlError::InvalidParam(_))
    ));
    assert!(matches!(
        ParamSet::from_core_value(&Value::Array(vec![Value::Float(f64::NAN)])),
        Err(SqlError::InvalidParam(_))
    ));

    // Strip leading @/: on map keys.
    let from_map = ParamSet::from_core_value(&Value::Map(vec![
        (Value::from("@id"), Value::Int(9)),
        (Value::from(":name"), Value::from("alex")),
    ]))
    .expect("map");
    assert_eq!(
        from_map,
        named(&[
            ("id", SqlValue::Int(9)),
            ("name", SqlValue::Text("alex".to_owned()))
        ])
    );
}

#[test]
fn sqlite_memory_crud_prepare_raw_execute_and_injection_safety() {
    let pool = SqlPool::from_connection_string("sqlite::memory:", SqlLimits::default())
        .expect("parse")
        .expect("pool");
    assert_eq!(pool.backend(), SqlBackend::Sqlite);
    assert!(pool.is_ready());
    pool.ping().expect("ping");

    pool.query(
        "CREATE TABLE players (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, cash INTEGER NOT NULL)",
        ParamSet::Empty,
    )
    .expect("create table");

    let id1 = pool
        .insert(
            "INSERT INTO players (name, cash) VALUES (?, ?)",
            ParamSet::Positional(vec![SqlValue::Text("Alice".to_owned()), SqlValue::Int(500)]),
        )
        .expect("insert 1");
    assert_eq!(id1, 1);

    // SQL injection payload treated strictly as bound string value.
    let injection = "Robert'); DROP TABLE players; --";
    let id2 = pool
        .insert(
            "INSERT INTO players (name, cash) VALUES (@name, :cash)",
            named(&[
                ("name", SqlValue::Text(injection.to_owned())),
                ("cash", SqlValue::Int(1250)),
            ]),
        )
        .expect("insert 2");
    assert_eq!(id2, 2);

    // Verify table intact and injection string stored verbatim.
    let fetched = pool
        .scalar(
            "SELECT name FROM players WHERE id = ?",
            ParamSet::Positional(vec![SqlValue::Int(2)]),
        )
        .expect("scalar");
    assert_eq!(fetched, Some(SqlValue::Text(injection.to_owned())));

    let single = pool
        .single(
            "SELECT id, name, cash FROM players WHERE id = @id",
            named(&[("id", SqlValue::Int(1))]),
        )
        .expect("single")
        .expect("row");
    assert_eq!(single.columns[1].1, SqlValue::Text("Alice".to_owned()));

    assert_eq!(
        pool.single(
            "SELECT id FROM players WHERE id = ?",
            ParamSet::Positional(vec![SqlValue::Int(999)]),
        )
        .expect("empty single"),
        None
    );

    let updated = pool
        .update(
            "UPDATE players SET cash = cash + ? WHERE id = ?",
            ParamSet::Positional(vec![SqlValue::Int(100), SqlValue::Int(1)]),
        )
        .expect("update");
    assert_eq!(updated, 1);

    // Query returning DML summary shape.
    let dml = pool
        .query(
            "UPDATE players SET cash = ? WHERE id = ?",
            ParamSet::Positional(vec![SqlValue::Int(777), SqlValue::Int(1)]),
        )
        .expect("dml query");
    let QueryResultSet::Dml(summary) = dml else {
        panic!("expected DML summary");
    };
    assert_eq!(summary.affected_rows, 1);
    assert_eq!(summary.changed_rows, 1);

    // Batch prepare.
    let batch_outcome = pool
        .prepare(
            "INSERT INTO players (name, cash) VALUES (?, ?)",
            PrepareParams::Batch(vec![
                ParamSet::Positional(vec![SqlValue::Text("Bob".to_owned()), SqlValue::Int(10)]),
                ParamSet::Positional(vec![SqlValue::Text("Cara".to_owned()), SqlValue::Int(20)]),
            ]),
        )
        .expect("batch prepare");
    let QueryOutcome::Batch(batch_results) = batch_outcome else {
        panic!("expected batch");
    };
    assert_eq!(batch_results.len(), 2);

    // RawExecute returns all 4 rows.
    let raw = pool
        .raw_execute("SELECT id, name FROM players ORDER BY id", ParamSet::Empty)
        .expect("raw");
    let QueryResultSet::Rows(rows) = raw else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 4);
}

#[test]
fn sqlite_transactions_commit_and_rollback_atomically() {
    let pool = SqlPool::from_connection_string("sqlite::memory:", SqlLimits::default())
        .expect("parse")
        .expect("pool");
    pool.query(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance INTEGER NOT NULL CHECK(balance >= 0))",
        ParamSet::Empty,
    )
    .expect("create");
    pool.insert(
        "INSERT INTO accounts (id, balance) VALUES (1, 100), (2, 50)",
        ParamSet::Empty,
    )
    .expect("seed");

    // Successful transfer.
    let ok = pool
        .transaction(vec![
            TransactionItem {
                sql: "UPDATE accounts SET balance = balance - @amt WHERE id = @from".to_owned(),
                params: named(&[("amt", SqlValue::Int(40)), ("from", SqlValue::Int(1))]),
            },
            TransactionItem {
                sql: "UPDATE accounts SET balance = balance + @amt WHERE id = @to".to_owned(),
                params: named(&[("amt", SqlValue::Int(40)), ("to", SqlValue::Int(2))]),
            },
        ])
        .expect("commit tx");
    assert!(ok);

    assert_eq!(
        pool.scalar("SELECT balance FROM accounts WHERE id = 1", ParamSet::Empty)
            .expect("b1"),
        Some(SqlValue::Int(60))
    );
    assert_eq!(
        pool.scalar("SELECT balance FROM accounts WHERE id = 2", ParamSet::Empty)
            .expect("b2"),
        Some(SqlValue::Int(90))
    );

    // Failing transaction (second statement violates CHECK constraint) must roll back first statement.
    let tx_err = pool
        .transaction(vec![
            TransactionItem {
                sql: "UPDATE accounts SET balance = balance + 1000 WHERE id = 2".to_owned(),
                params: ParamSet::Empty,
            },
            TransactionItem {
                sql: "UPDATE accounts SET balance = balance - 999 WHERE id = 1".to_owned(),
                params: ParamSet::Empty,
            },
        ])
        .expect_err("must fail check constraint");
    assert!(matches!(tx_err, SqlError::Backend(_)));

    assert_eq!(
        pool.scalar("SELECT balance FROM accounts WHERE id = 1", ParamSet::Empty)
            .expect("b1 unchanged"),
        Some(SqlValue::Int(60))
    );
    assert_eq!(
        pool.scalar("SELECT balance FROM accounts WHERE id = 2", ParamSet::Empty)
            .expect("b2 unchanged"),
        Some(SqlValue::Int(90))
    );
}

#[test]
fn sqlite_transaction_rejects_an_explicit_commit() {
    let pool = SqlPool::from_connection_string("sqlite::memory:", SqlLimits::default())
        .expect("parse")
        .expect("pool");
    pool.query("CREATE TABLE entries (value INTEGER)", ParamSet::Empty)
        .expect("create");
    let error = pool
        .transaction(vec![
            TransactionItem {
                sql: "INSERT INTO entries VALUES (1)".to_owned(),
                params: ParamSet::Empty,
            },
            TransactionItem {
                sql: "/* managed transaction */ COMMIT".to_owned(),
                params: ParamSet::Empty,
            },
        ])
        .expect_err("explicit commit must be refused");
    assert!(matches!(error, SqlError::InvalidSql(_)));
    assert_eq!(
        pool.scalar("SELECT COUNT(*) FROM entries", ParamSet::Empty)
            .expect("count"),
        Some(SqlValue::Int(0))
    );
}

#[test]
fn sqlite_file_wal_persistence_and_limits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("nested").join("game.db");
    let uri = format!("sqlite://{}", db_path.display());

    let limits = SqlLimits {
        max_rows: 5,
        max_result_bytes: 65_536,
        timeout: Duration::from_millis(120),
        pool_size: 4,
        ..SqlLimits::default()
    };

    {
        let mut pool = SqlPool::from_connection_string(&uri, limits)
            .expect("parse")
            .expect("pool");
        let mode = pool
            .scalar("PRAGMA journal_mode", ParamSet::Empty)
            .expect("pragma");
        assert_eq!(mode, Some(SqlValue::Text("wal".to_owned())));

        pool.query(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, payload TEXT NOT NULL)",
            ParamSet::Empty,
        )
        .expect("create");

        for i in 1..=6 {
            pool.insert(
                "INSERT INTO items (id, payload) VALUES (?, ?)",
                ParamSet::Positional(vec![SqlValue::Int(i), SqlValue::Text(format!("item-{i}"))]),
            )
            .expect("insert");
        }

        // Row limit exceeded (6 rows > max_rows 5).
        let row_err = pool
            .query("SELECT id, payload FROM items", ParamSet::Empty)
            .expect_err("row limit");
        assert_eq!(
            row_err,
            SqlError::ResultTooLarge {
                kind: LimitKind::Rows,
                limit: 5
            }
        );

        // Timeout via infinite recursive CTE.
        let timeout_err = pool
            .scalar(
                "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt) SELECT count(*) FROM cnt",
                ParamSet::Empty,
            )
            .expect_err("cte timeout");
        assert!(matches!(timeout_err, SqlError::Timeout(_)));

        pool.shutdown();
    }

    // Reopen with larger row limit to verify persistence and byte cap.
    let pool2 = SqlPool::from_connection_string(
        &uri,
        SqlLimits {
            max_rows: 100,
            max_result_bytes: 65_536,
            ..SqlLimits::default()
        },
    )
    .expect("reopen parse")
    .expect("reopen pool");

    assert_eq!(
        pool2
            .scalar("SELECT count(*) FROM items", ParamSet::Empty)
            .expect("persisted count"),
        Some(SqlValue::Int(6))
    );

    // Byte limit exceeded: insert 4 rows x 20_000 bytes = 80_000 > 65_536.
    let big = "x".repeat(20_000);
    for i in 10..14 {
        pool2
            .insert(
                "INSERT INTO items (id, payload) VALUES (?, ?)",
                ParamSet::Positional(vec![SqlValue::Int(i), SqlValue::Text(big.clone())]),
            )
            .expect("insert big");
    }
    let byte_err = pool2
        .query(
            "SELECT id, payload FROM items WHERE id >= 10",
            ParamSet::Empty,
        )
        .expect_err("byte limit");
    assert_eq!(
        byte_err,
        SqlError::ResultTooLarge {
            kind: LimitKind::Bytes,
            limit: 65_536
        }
    );
}

#[test]
fn sqlite_execute_async_runs_off_caller_thread_and_supports_cancellation() {
    let pool = SqlPool::from_connection_string("sqlite::memory:", SqlLimits::default())
        .expect("parse")
        .expect("pool");
    let caller_thread = thread::current().id();
    let (tx, rx) = mpsc::sync_channel(1);

    pool.execute_async(
        SqlOperation::Scalar {
            sql: "SELECT 40 + 2".to_owned(),
            params: ParamSet::Empty,
        },
        move |res| {
            let callback_thread = thread::current().id();
            let _ = tx.send((callback_thread, res));
        },
    )
    .expect("dispatch");

    let (callback_thread, res) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("callback arrived");
    assert_ne!(callback_thread, caller_thread);
    assert_eq!(
        res.expect("ok"),
        QueryOutcome::Scalar(Some(SqlValue::Int(42)))
    );

    // Pre-cancelled operation returns SqlError::Cancelled.
    let cancelled = Arc::new(AtomicBool::new(true));
    let (tx2, rx2) = mpsc::sync_channel(1);
    pool.execute_async_cancellable(
        SqlOperation::Scalar {
            sql: "SELECT 1".to_owned(),
            params: ParamSet::Empty,
        },
        cancelled,
        move |res| {
            let _ = tx2.send(res);
        },
    )
    .expect("dispatch cancelled");
    assert_eq!(
        rx2.recv_timeout(Duration::from_secs(5)).expect("recv"),
        Err(SqlError::Cancelled)
    );
}

#[test]
fn core_value_conversion_and_oxmysql_shapes() {
    let batch = PrepareParams::from_core_value(&Value::Array(vec![
        Value::Array(vec![Value::Int(1), Value::from("a")]),
        Value::Array(vec![Value::Int(2), Value::from("b")]),
    ]))
    .expect("batch");
    assert!(matches!(batch, PrepareParams::Batch(items) if items.len() == 2));

    let pool = SqlPool::from_connection_string("sqlite::memory:", SqlLimits::default())
        .expect("parse")
        .expect("pool");
    pool.query(
        "CREATE TABLE kv (id INTEGER PRIMARY KEY AUTOINCREMENT, k TEXT NOT NULL)",
        ParamSet::Empty,
    )
    .expect("create");

    let dml = pool
        .execute(SqlOperation::Query {
            sql: "INSERT INTO kv (k) VALUES (?)".to_owned(),
            params: ParamSet::Positional(vec![SqlValue::Text("hello".to_owned())]),
        })
        .expect("dml")
        .into_core_value();
    assert_eq!(
        dml,
        Value::Map(vec![
            (Value::from("affectedRows"), Value::Int(1)),
            (Value::from("insertId"), Value::Int(1)),
            (Value::from("changedRows"), Value::Int(1)),
        ])
    );

    let rows_val = pool
        .execute(SqlOperation::Query {
            sql: "SELECT id, k FROM kv".to_owned(),
            params: ParamSet::Empty,
        })
        .expect("rows")
        .into_core_value();
    assert_eq!(
        rows_val,
        Value::Array(vec![Value::Map(vec![
            (Value::from("id"), Value::Int(1)),
            (Value::from("k"), Value::from("hello")),
        ])])
    );

    let nil_single = pool
        .execute(SqlOperation::Single {
            sql: "SELECT id FROM kv WHERE id = 999".to_owned(),
            params: ParamSet::Empty,
        })
        .expect("nil single")
        .into_core_value();
    assert_eq!(nil_single, Value::Nil);
}

#[test]
fn review_regressions_sqlite_backslashes_and_quoted_key_value_config() {
    // SQLite treats `\` inside string literals as a literal byte, not an escape.
    let parsed_sqlite =
        ParsedSql::parse_for("SELECT 'C:\\' AS dir, ? AS val", Dialect::Sqlite).expect("sqlite");
    assert_eq!(parsed_sqlite.placeholder_count(), 1);
    assert!(matches!(
        ParsedSql::parse_for("SELECT 'C:\\' AS dir, ? AS val", Dialect::Mysql),
        Err(SqlError::InvalidSql(_))
    ));

    let pool =
        SqlPool::from_connection_string("sqlite:///:memory:?cache=shared", SqlLimits::default())
            .expect("sqlite:///:memory:?cache=shared")
            .expect("pool");
    let row = pool
        .single(
            "SELECT 'C:\\' AS dir, ? AS val",
            ParamSet::Positional(vec![SqlValue::Int(7)]),
        )
        .expect("single")
        .expect("row");
    assert_eq!(row.columns[0].1, SqlValue::Text("C:\\".to_owned()));
    assert_eq!(row.columns[1].1, SqlValue::Int(7));

    // Quoted `;` inside password and connectTimeout=0 + non-leaking InvalidConfig errors.
    let cfg = DatabaseConfig::parse("host=db.local;pwd=\"pa;ss\";db=app;connectTimeout=0")
        .expect("quoted semicolon");
    let Some(DatabaseConfig::Mysql(mysql)) = cfg else {
        panic!("expected mysql config");
    };
    assert_eq!(mysql.password.as_deref(), Some("pa;ss"));
    assert_eq!(mysql.connect_timeout, None);

    let err = DatabaseConfig::parse("password=supersecret_bad_segment;broken")
        .expect_err("missing equals")
        .to_string();
    assert!(!err.contains("supersecret"));
    assert!(!err.contains("broken"));
}
