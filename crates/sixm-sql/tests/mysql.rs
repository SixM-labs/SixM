//! MySQL / MariaDB integration tests gated by `SIXM_TEST_MYSQL_URL`.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sixm_sql::{
    LimitKind, ParamSet, PrepareParams, QueryOutcome, QueryResultSet, SqlBackend, SqlError,
    SqlLimits, SqlOperation, SqlPool, SqlValue, TransactionItem,
};

fn named(pairs: &[(&str, SqlValue)]) -> ParamSet {
    let mut map = BTreeMap::new();
    for (k, v) in pairs {
        map.insert((*k).to_owned(), v.clone());
    }
    ParamSet::Named(map)
}

#[test]
fn mysql_crud_transactions_limits_and_async() {
    let Ok(url) = std::env::var("SIXM_TEST_MYSQL_URL") else {
        eprintln!("skipping mysql integration tests: SIXM_TEST_MYSQL_URL is not set");
        return;
    };
    if url.trim().is_empty() {
        eprintln!("skipping mysql integration tests: SIXM_TEST_MYSQL_URL is empty");
        return;
    }

    let mut pool = SqlPool::from_connection_string(&url, SqlLimits::default())
        .expect("parse mysql url")
        .expect("mysql pool");
    assert_eq!(pool.backend(), SqlBackend::Mysql);
    assert!(pool.is_ready());
    pool.ping().expect("mysql ping");

    pool.query("DROP TABLE IF EXISTS sixm_sql_e2_test", ParamSet::Empty)
        .expect("drop old table");
    pool.query(
        "CREATE TABLE sixm_sql_e2_test (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            name VARCHAR(255) NOT NULL,
            score BIGINT NOT NULL
        )",
        ParamSet::Empty,
    )
    .expect("create table");

    let (id1, id2) = verify_crud_and_prepare(&pool);
    verify_transactions(&pool, id1, id2);
    verify_async_and_limits(&pool, &url);

    pool.query("DROP TABLE IF EXISTS sixm_sql_e2_test", ParamSet::Empty)
        .expect("cleanup");
    pool.shutdown();
}

fn verify_crud_and_prepare(pool: &SqlPool) -> (u64, u64) {
    let id1 = pool
        .insert(
            "INSERT INTO sixm_sql_e2_test (name, score) VALUES (?, ?)",
            ParamSet::Positional(vec![SqlValue::Text("Alice".to_owned()), SqlValue::Int(100)]),
        )
        .expect("insert 1");
    assert!(id1 >= 1);

    let injection = "Mallory'); DROP TABLE sixm_sql_e2_test; --";
    let id2 = pool
        .insert(
            "INSERT INTO sixm_sql_e2_test (name, score) VALUES (@name, :score)",
            named(&[
                ("name", SqlValue::Text(injection.to_owned())),
                ("score", SqlValue::Int(200)),
            ]),
        )
        .expect("insert 2");
    assert!(id2 > id1);

    assert_eq!(
        pool.scalar(
            "SELECT name FROM sixm_sql_e2_test WHERE id = @id",
            named(&[("id", SqlValue::UInt(id2))]),
        )
        .expect("scalar"),
        Some(SqlValue::Text(injection.to_owned()))
    );

    let updated = pool
        .update(
            "UPDATE sixm_sql_e2_test SET score = score + ? WHERE id = ?",
            ParamSet::Positional(vec![SqlValue::Int(50), SqlValue::UInt(id1)]),
        )
        .expect("update");
    assert_eq!(updated, 1);

    let batch = pool
        .prepare(
            "INSERT INTO sixm_sql_e2_test (name, score) VALUES (?, ?)",
            PrepareParams::Batch(vec![
                ParamSet::Positional(vec![SqlValue::Text("Bob".to_owned()), SqlValue::Int(10)]),
                ParamSet::Positional(vec![SqlValue::Text("Cara".to_owned()), SqlValue::Int(20)]),
            ]),
        )
        .expect("batch prepare");
    assert!(matches!(batch, QueryOutcome::Batch(items) if items.len() == 2));

    let single = pool
        .single(
            "SELECT id, name, score FROM sixm_sql_e2_test WHERE id = ?",
            ParamSet::Positional(vec![SqlValue::UInt(id1)]),
        )
        .expect("single")
        .expect("row");
    assert_eq!(single.columns[2].1, SqlValue::Int(150));

    (id1, id2)
}

fn verify_transactions(pool: &SqlPool, id1: u64, id2: u64) {
    assert!(
        pool.transaction(vec![
            TransactionItem {
                sql: "UPDATE sixm_sql_e2_test SET score = score - 10 WHERE id = @id".to_owned(),
                params: named(&[("id", SqlValue::UInt(id1))]),
            },
            TransactionItem {
                sql: "UPDATE sixm_sql_e2_test SET score = score + 10 WHERE id = @id".to_owned(),
                params: named(&[("id", SqlValue::UInt(id2))]),
            },
        ])
        .expect("tx commit")
    );

    let tx_err = pool
        .transaction(vec![
            TransactionItem {
                sql: "UPDATE sixm_sql_e2_test SET score = score + 9999 WHERE id = @id".to_owned(),
                params: named(&[("id", SqlValue::UInt(id1))]),
            },
            TransactionItem {
                sql: "INSERT INTO sixm_sql_e2_test (name, score) VALUES (NULL, 1)".to_owned(),
                params: ParamSet::Empty,
            },
        ])
        .expect_err("tx rollback");
    assert!(matches!(tx_err, SqlError::Backend(_)));
    assert_eq!(
        pool.scalar(
            "SELECT score FROM sixm_sql_e2_test WHERE id = ?",
            ParamSet::Positional(vec![SqlValue::UInt(id1)]),
        )
        .expect("score unchanged"),
        Some(SqlValue::Int(140))
    );
}

fn verify_async_and_limits(pool: &SqlPool, url: &str) {
    let caller_thread = thread::current().id();
    let (tx, rx) = mpsc::sync_channel(1);
    pool.execute_async(
        SqlOperation::Query {
            sql: "SELECT id, name FROM sixm_sql_e2_test ORDER BY id".to_owned(),
            params: ParamSet::Empty,
        },
        move |res| {
            let _ = tx.send((thread::current().id(), res));
        },
    )
    .expect("async dispatch");
    let (cb_thread, outcome) = rx.recv_timeout(Duration::from_secs(5)).expect("cb");
    assert_ne!(cb_thread, caller_thread);
    assert!(matches!(
        outcome.expect("ok"),
        QueryOutcome::Query(QueryResultSet::Rows(rows)) if rows.len() == 4
    ));

    let mut limited = SqlPool::from_connection_string(
        url,
        SqlLimits {
            max_rows: 2,
            timeout: Duration::from_millis(250),
            ..SqlLimits::default()
        },
    )
    .expect("limited parse")
    .expect("limited pool");

    let row_err = limited
        .query("SELECT id FROM sixm_sql_e2_test", ParamSet::Empty)
        .expect_err("row limit");
    assert_eq!(
        row_err,
        SqlError::ResultTooLarge {
            kind: LimitKind::Rows,
            limit: 2
        }
    );

    let timeout_err = limited
        .scalar("SELECT SLEEP(2)", ParamSet::Empty)
        .expect_err("timeout");
    assert!(matches!(timeout_err, SqlError::Timeout(_)));

    limited.shutdown();
}
